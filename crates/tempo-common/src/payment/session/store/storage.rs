//! `SQLite` CRUD operations for channel persistence.

use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use alloy::primitives::Address;
use rusqlite::{params, types::Type};

use crate::error::{PaymentError, TempoError};

use super::model::{ChannelRecord, ChannelStatus};

type ChannelStoreResult<T> = std::result::Result<T, TempoError>;

fn store_error<E>(operation: &'static str, source: E) -> TempoError
where
    E: Error + Send + Sync + 'static,
{
    PaymentError::ChannelPersistenceSource {
        operation,
        source: Box::new(source),
    }
    .into()
}

fn integer_conversion_error(field: &'static str, value: u64) -> TempoError {
    store_error(
        "serialize channel integer field",
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{field} value {value} exceeds i64::MAX"),
        ),
    )
}

fn to_i64_checked(value: u64, field: &'static str) -> ChannelStoreResult<i64> {
    i64::try_from(value).map_err(|_| integer_conversion_error(field, value))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChannelStoreDiagnostics {
    pub malformed_load_drops: u64,
    pub malformed_list_drops: u64,
}

static MALFORMED_LOAD_DROPS: AtomicU64 = AtomicU64::new(0);
static MALFORMED_LIST_DROPS: AtomicU64 = AtomicU64::new(0);

fn wallet_dir() -> ChannelStoreResult<PathBuf> {
    Ok(crate::tempo_home()?.join("wallet"))
}

pub(super) fn ensure_wallet_dir() -> ChannelStoreResult<PathBuf> {
    let dir = wallet_dir()?;
    fs::create_dir_all(&dir).map_err(|err| store_error("ensure channel wallet dir", err))?;
    Ok(dir)
}

fn open_db() -> ChannelStoreResult<rusqlite::Connection> {
    let dir = ensure_wallet_dir()?;
    let db_path = dir.join("channels.db");
    open_db_at(&db_path)
}

fn open_db_at(path: &Path) -> ChannelStoreResult<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)
        .map_err(|err| store_error("open channels database", err))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| store_error("set channels database permissions", err))?;
    }
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(|err| store_error("configure channels database pragmas", err))?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS channels (
            channel_id         TEXT PRIMARY KEY,
            version            INTEGER NOT NULL DEFAULT 1,
            origin             TEXT NOT NULL,
            request_url        TEXT NOT NULL DEFAULT '',
            chain_id           INTEGER NOT NULL,
            escrow_contract    TEXT NOT NULL,
            token              TEXT NOT NULL,
            payee              TEXT NOT NULL,
            payer              TEXT NOT NULL,
            authorized_signer  TEXT NOT NULL,
            salt               TEXT NOT NULL,
            session_protocol   TEXT NOT NULL DEFAULT 'v1',
            descriptor_json    TEXT,
            deposit            TEXT NOT NULL,
            cumulative_amount  TEXT NOT NULL,
            challenge_echo     TEXT NOT NULL,
            state              TEXT NOT NULL DEFAULT 'active',
            close_requested_at INTEGER NOT NULL DEFAULT 0,
            grace_ready_at     INTEGER NOT NULL DEFAULT 0,
            created_at         INTEGER NOT NULL,
            last_used_at       INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_channels_origin ON channels(origin);",
    )
    .map_err(|err| store_error("create channels schema", err))?;

    // Migration: add accepted_cumulative column for existing databases.
    match conn.execute_batch(
        "ALTER TABLE channels ADD COLUMN accepted_cumulative TEXT NOT NULL DEFAULT '0';",
    ) {
        Ok(()) => {}
        Err(e) if e.to_string().contains("duplicate column name") => {}
        Err(e) => return Err(store_error("migrate channels schema", e)),
    }

    // Migration: add server_spent column for existing databases.
    match conn
        .execute_batch("ALTER TABLE channels ADD COLUMN server_spent TEXT NOT NULL DEFAULT '0';")
    {
        Ok(()) => {}
        Err(e) if e.to_string().contains("duplicate column name") => {}
        Err(e) => return Err(store_error("migrate channels schema (server_spent)", e)),
    }

    match conn.execute_batch(
        "ALTER TABLE channels ADD COLUMN session_protocol TEXT NOT NULL DEFAULT 'v1';",
    ) {
        Ok(()) => {}
        Err(e) if e.to_string().contains("duplicate column name") => {}
        Err(e) => return Err(store_error("migrate channels schema (session_protocol)", e)),
    }

    match conn.execute_batch("ALTER TABLE channels ADD COLUMN descriptor_json TEXT;") {
        Ok(()) => {}
        Err(e) if e.to_string().contains("duplicate column name") => {}
        Err(e) => return Err(store_error("migrate channels schema (descriptor_json)", e)),
    }

    Ok(conn)
}

fn decode_u64_column(
    row: &rusqlite::Row<'_>,
    index: usize,
    column: &'static str,
) -> rusqlite::Result<u64> {
    let raw = row.get::<_, i64>(index)?;
    u64::try_from(raw).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            Type::Integer,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("negative value for {column}: {raw}"),
            )),
        )
    })
}

fn decode_u128_column(
    row: &rusqlite::Row<'_>,
    index: usize,
    column: &'static str,
) -> rusqlite::Result<u128> {
    let raw = row.get::<_, String>(index)?;
    raw.parse::<u128>().map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid {column} value '{raw}': {err}"),
            )),
        )
    })
}

fn decode_address_column(
    row: &rusqlite::Row<'_>,
    index: usize,
    column: &'static str,
) -> rusqlite::Result<Address> {
    let raw = row.get::<_, String>(index)?;
    raw.parse::<Address>().map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid {column} value '{raw}': {err}"),
            )),
        )
    })
}

fn decode_channel_status(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<ChannelStatus> {
    let state_value = row.get::<_, String>(index)?;
    ChannelStatus::try_from_db_str(&state_value)
        .map_err(|err| rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(err)))
}

fn map_channel_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChannelRecord> {
    let state = decode_channel_status(row, 17)?;

    let mut record = ChannelRecord {
        version: row.get::<_, u32>(0)?,
        origin: row.get(1)?,
        request_url: row.get(2)?,
        chain_id: decode_u64_column(row, 3, "chain_id")?,
        escrow_contract: decode_address_column(row, 4, "escrow_contract")?,
        token: row.get(5)?,
        payee: row.get(6)?,
        payer: row.get(7)?,
        authorized_signer: decode_address_column(row, 8, "authorized_signer")?,
        salt: row.get(9)?,
        channel_id: row.get::<_, String>(10)?.parse().map_err(|source| {
            rusqlite::Error::FromSqlConversionFailure(10, Type::Text, Box::new(source))
        })?,
        session_protocol: row.get(11)?,
        descriptor_json: row.get(12)?,
        deposit: decode_u128_column(row, 13, "deposit")?,
        cumulative_amount: decode_u128_column(row, 14, "cumulative_amount")?,
        accepted_cumulative: decode_u128_column(row, 15, "accepted_cumulative")?,
        challenge_echo: row.get(16)?,
        state,
        close_requested_at: decode_u64_column(row, 18, "close_requested_at")?,
        grace_ready_at: decode_u64_column(row, 19, "grace_ready_at")?,
        created_at: decode_u64_column(row, 20, "created_at")?,
        last_used_at: decode_u64_column(row, 21, "last_used_at")?,
        server_spent: decode_u128_column(row, 22, "server_spent")?,
    };

    if !record.normalize_persisted_identity() {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            5,
            Type::Text,
            Box::new(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid channel token or payee address",
            )),
        ));
    }

    Ok(record)
}

const fn is_malformed_channel_row_error(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::FromSqlConversionFailure(_, _, _)
            | rusqlite::Error::InvalidColumnType(_, _, _)
            | rusqlite::Error::IntegralValueOutOfRange(_, _)
    )
}

fn save_channel_in_conn(
    conn: &rusqlite::Connection,
    record: &ChannelRecord,
) -> ChannelStoreResult<()> {
    let chain_id = to_i64_checked(record.chain_id, "chain_id")?;
    let close_requested_at = to_i64_checked(record.close_requested_at, "close_requested_at")?;
    let grace_ready_at = to_i64_checked(record.grace_ready_at, "grace_ready_at")?;
    let created_at = to_i64_checked(record.created_at, "created_at")?;
    let last_used_at = to_i64_checked(record.last_used_at, "last_used_at")?;

    conn.execute(
        "INSERT INTO channels (
            channel_id, version, origin, request_url, chain_id,
            escrow_contract, token, payee, payer, authorized_signer,
            salt, session_protocol, descriptor_json, deposit, cumulative_amount, accepted_cumulative, challenge_echo,
            state, close_requested_at, grace_ready_at, created_at, last_used_at,
            server_spent
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)
        ON CONFLICT(channel_id) DO UPDATE SET
            version = excluded.version,
            origin = excluded.origin,
            request_url = excluded.request_url,
            chain_id = excluded.chain_id,
            escrow_contract = excluded.escrow_contract,
            token = excluded.token,
            payee = excluded.payee,
            payer = excluded.payer,
            authorized_signer = excluded.authorized_signer,
            salt = excluded.salt,
            session_protocol = excluded.session_protocol,
            descriptor_json = excluded.descriptor_json,
            deposit = CASE
                WHEN LENGTH(channels.deposit) > LENGTH(excluded.deposit) THEN channels.deposit
                WHEN LENGTH(channels.deposit) < LENGTH(excluded.deposit) THEN excluded.deposit
                WHEN channels.deposit >= excluded.deposit THEN channels.deposit
                ELSE excluded.deposit
            END,
            cumulative_amount = CASE
                WHEN LENGTH(channels.cumulative_amount) > LENGTH(excluded.cumulative_amount) THEN channels.cumulative_amount
                WHEN LENGTH(channels.cumulative_amount) < LENGTH(excluded.cumulative_amount) THEN excluded.cumulative_amount
                WHEN channels.cumulative_amount >= excluded.cumulative_amount THEN channels.cumulative_amount
                ELSE excluded.cumulative_amount
            END,
            accepted_cumulative = CASE
                WHEN LENGTH(channels.accepted_cumulative) > LENGTH(excluded.accepted_cumulative) THEN channels.accepted_cumulative
                WHEN LENGTH(channels.accepted_cumulative) < LENGTH(excluded.accepted_cumulative) THEN excluded.accepted_cumulative
                WHEN channels.accepted_cumulative >= excluded.accepted_cumulative THEN channels.accepted_cumulative
                ELSE excluded.accepted_cumulative
            END,
            challenge_echo = excluded.challenge_echo,
            state = excluded.state,
            close_requested_at = excluded.close_requested_at,
            grace_ready_at = excluded.grace_ready_at,
            created_at = channels.created_at,
            last_used_at = excluded.last_used_at,
            server_spent = excluded.server_spent",
        params![
            record.channel_id_hex(),
            record.version,
            record.origin,
            record.request_url,
            chain_id,
            format!("{:#x}", record.escrow_contract),
            record.token,
            record.payee,
            record.payer,
            format!("{:#x}", record.authorized_signer),
            record.salt,
            record.session_protocol_or_legacy(),
            record.descriptor_json.as_deref(),
            record.deposit.to_string(),
            record.cumulative_amount.to_string(),
            record.accepted_cumulative.to_string(),
            record.challenge_echo,
            record.state.as_str(),
            close_requested_at,
            grace_ready_at,
            created_at,
            last_used_at,
            record.server_spent.to_string(),
        ],
    )
    .map_err(|err| store_error("save channel", err))?;
    Ok(())
}

fn load_channel_in_conn(
    conn: &rusqlite::Connection,
    channel_id: &str,
) -> ChannelStoreResult<Option<ChannelRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT version, origin, request_url, chain_id,
                    escrow_contract, token, payee, payer, authorized_signer,
                    salt, channel_id, session_protocol, descriptor_json, deposit, cumulative_amount, accepted_cumulative,
                    challenge_echo, state, close_requested_at, grace_ready_at, created_at, last_used_at,
                    server_spent
             FROM channels WHERE LOWER(channel_id) = LOWER(?1)",
        )
        .map_err(|err| store_error("prepare channel load query", err))?;

    let result = stmt.query_row(params![channel_id], map_channel_row);
    match result {
        Ok(record) => Ok(Some(record)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) if is_malformed_channel_row_error(&e) => {
            MALFORMED_LOAD_DROPS.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(channel_id, error = %e, "Skipping malformed channel row while loading");
            Ok(None)
        }
        Err(e) => Err(store_error("load channel", e)),
    }
}

fn find_reusable_channel_in_conn(
    conn: &rusqlite::Connection,
    origin: &str,
    payer: &str,
    escrow_contract: Address,
    token: &str,
    payee: &str,
    chain_id: u64,
) -> ChannelStoreResult<Option<ChannelRecord>> {
    let mut stmt = conn
        .prepare(
            "SELECT version, origin, request_url, chain_id,
                    escrow_contract, token, payee, payer, authorized_signer,
                    salt, channel_id, session_protocol, descriptor_json, deposit, cumulative_amount, accepted_cumulative,
                    challenge_echo, state, close_requested_at, grace_ready_at, created_at, last_used_at,
                    server_spent
             FROM channels
             WHERE origin = ?1 AND payer = ?2 AND escrow_contract = ?3
               AND token = ?4 AND payee = ?5 AND chain_id = ?6
               AND state = 'active'
             ORDER BY last_used_at DESC LIMIT 1",
        )
        .map_err(|err| store_error("prepare reusable channel query", err))?;

    let chain_id_i64 = to_i64_checked(chain_id, "chain_id")?;
    let escrow_hex = format!("{escrow_contract:#x}");
    let result = stmt.query_row(
        params![origin, payer, escrow_hex, token, payee, chain_id_i64],
        map_channel_row,
    );

    match result {
        Ok(record) => Ok(Some(record)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(store_error("find reusable channel", e)),
    }
}

fn delete_channel_in_conn(conn: &rusqlite::Connection, channel_id: &str) -> ChannelStoreResult<()> {
    conn.execute(
        "DELETE FROM channels WHERE LOWER(channel_id) = LOWER(?1)",
        params![channel_id],
    )
    .map_err(|err| store_error("delete channel", err))?;
    Ok(())
}

fn update_channel_close_state_in_conn(
    conn: &rusqlite::Connection,
    channel_id: &str,
    state: ChannelStatus,
    close_requested_at: u64,
    grace_ready_at: u64,
) -> ChannelStoreResult<()> {
    let close_requested_at = to_i64_checked(close_requested_at, "close_requested_at")?;
    let grace_ready_at = to_i64_checked(grace_ready_at, "grace_ready_at")?;
    conn.execute(
        "UPDATE channels SET state = ?1, close_requested_at = ?2, grace_ready_at = ?3 WHERE LOWER(channel_id) = LOWER(?4)",
        params![state.as_str(), close_requested_at, grace_ready_at, channel_id],
    )
    .map_err(|err| store_error("update channel close state", err))?;
    Ok(())
}

fn update_channel_cumulative_floor_in_conn(
    conn: &rusqlite::Connection,
    channel_id: &str,
    accepted_cumulative: u128,
) -> ChannelStoreResult<()> {
    let accepted = accepted_cumulative.to_string();
    conn.execute(
        "UPDATE channels
         SET cumulative_amount = CASE
             WHEN LENGTH(cumulative_amount) > LENGTH(?1) THEN cumulative_amount
             WHEN LENGTH(cumulative_amount) < LENGTH(?1) THEN ?1
             WHEN cumulative_amount >= ?1 THEN cumulative_amount
             ELSE ?1
         END,
         accepted_cumulative = CASE
             WHEN LENGTH(accepted_cumulative) > LENGTH(?1) THEN accepted_cumulative
             WHEN LENGTH(accepted_cumulative) < LENGTH(?1) THEN ?1
             WHEN accepted_cumulative >= ?1 THEN accepted_cumulative
             ELSE ?1
         END
         WHERE LOWER(channel_id) = LOWER(?2)",
        params![accepted, channel_id],
    )
    .map_err(|err| store_error("update channel cumulative floor", err))?;
    Ok(())
}

pub fn save_channel(record: &ChannelRecord) -> ChannelStoreResult<()> {
    let conn = open_db()?;
    save_channel_in_conn(&conn, record)
}

pub fn load_channel(channel_id: &str) -> ChannelStoreResult<Option<ChannelRecord>> {
    let conn = open_db()?;
    load_channel_in_conn(&conn, channel_id)
}

pub fn load_channels_by_origin(origin: &str) -> ChannelStoreResult<Vec<ChannelRecord>> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT version, origin, request_url, chain_id,
                    escrow_contract, token, payee, payer, authorized_signer,
                    salt, channel_id, session_protocol, descriptor_json, deposit, cumulative_amount, accepted_cumulative,
                    challenge_echo, state, close_requested_at, grace_ready_at, created_at, last_used_at,
                    server_spent
             FROM channels WHERE origin = ?1 ORDER BY last_used_at DESC",
        )
        .map_err(|err| store_error("prepare channels load by origin query", err))?;

    let rows = stmt
        .query_map(params![origin], map_channel_row)
        .map_err(|err| store_error("load channels by origin", err))?;

    let mut channels = Vec::new();
    let mut dropped_rows = 0usize;
    for row in rows {
        match row {
            Ok(channel) => channels.push(channel),
            Err(err) => {
                dropped_rows += 1;
                tracing::warn!(
                    "Skipping malformed channel row while loading channels by origin: {err}"
                );
            }
        }
    }

    if dropped_rows > 0 {
        MALFORMED_LIST_DROPS.fetch_add(dropped_rows as u64, Ordering::Relaxed);
    }

    Ok(channels)
}

pub fn find_reusable_channel(
    origin: &str,
    payer: &str,
    escrow_contract: Address,
    token: &str,
    payee: &str,
    chain_id: u64,
) -> ChannelStoreResult<Option<ChannelRecord>> {
    let conn = open_db()?;
    find_reusable_channel_in_conn(
        &conn,
        origin,
        payer,
        escrow_contract,
        token,
        payee,
        chain_id,
    )
}

pub fn delete_channel(channel_id: &str) -> ChannelStoreResult<()> {
    let conn = open_db()?;
    delete_channel_in_conn(&conn, channel_id)
}

pub fn list_channels() -> ChannelStoreResult<Vec<ChannelRecord>> {
    let conn = open_db()?;
    let mut stmt = conn
        .prepare(
            "SELECT version, origin, request_url, chain_id,
                    escrow_contract, token, payee, payer, authorized_signer,
                    salt, channel_id, session_protocol, descriptor_json, deposit, cumulative_amount, accepted_cumulative,
                    challenge_echo, state, close_requested_at, grace_ready_at, created_at, last_used_at,
                    server_spent
             FROM channels
             WHERE origin <> 'http://127.0.0.1'
             ORDER BY last_used_at DESC",
        )
        .map_err(|err| store_error("prepare channels list query", err))?;

    let rows = stmt
        .query_map([], map_channel_row)
        .map_err(|err| store_error("list channels", err))?;

    let mut channels = Vec::new();
    let mut dropped_rows = 0usize;
    for row in rows {
        match row {
            Ok(channel) => channels.push(channel),
            Err(err) => {
                dropped_rows += 1;
                tracing::warn!("Skipping malformed channel row while listing channels: {err}");
            }
        }
    }

    if dropped_rows > 0 {
        MALFORMED_LIST_DROPS.fetch_add(dropped_rows as u64, Ordering::Relaxed);
    }

    Ok(channels)
}

pub fn take_channel_store_diagnostics() -> ChannelStoreDiagnostics {
    ChannelStoreDiagnostics {
        malformed_load_drops: MALFORMED_LOAD_DROPS.swap(0, Ordering::Relaxed),
        malformed_list_drops: MALFORMED_LIST_DROPS.swap(0, Ordering::Relaxed),
    }
}

pub fn update_channel_close_state(
    channel_id: &str,
    state: ChannelStatus,
    close_requested_at: u64,
    grace_ready_at: u64,
) -> ChannelStoreResult<()> {
    let conn = open_db()?;
    update_channel_close_state_in_conn(&conn, channel_id, state, close_requested_at, grace_ready_at)
}

pub fn update_channel_cumulative_floor(
    channel_id: &str,
    accepted_cumulative: u128,
) -> ChannelStoreResult<()> {
    let conn = open_db()?;
    update_channel_cumulative_floor_in_conn(&conn, channel_id, accepted_cumulative)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn sample_record(
        channel_id: alloy::primitives::B256,
        last_used_at: u64,
        state: ChannelStatus,
    ) -> ChannelRecord {
        ChannelRecord {
            version: 1,
            origin: "https://api.example.com".to_string(),
            request_url: "https://api.example.com/v1/chat".to_string(),
            chain_id: 4217,
            escrow_contract: "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            token: "0x0000000000000000000000000000000000000002".to_string(),
            payee: "0x0000000000000000000000000000000000000003".to_string(),
            payer: "0x0000000000000000000000000000000000000004".to_string(),
            authorized_signer: "0x0000000000000000000000000000000000000005"
                .parse()
                .unwrap(),
            salt: "0x01".to_string(),
            channel_id,
            session_protocol: mpp::protocol::methods::tempo::session::SESSION_PROTOCOL_LEGACY
                .to_string(),
            descriptor_json: None,
            deposit: 1_000,
            cumulative_amount: 10,
            accepted_cumulative: 0,
            server_spent: 0,
            challenge_echo: "{}".to_string(),
            state,
            close_requested_at: 0,
            grace_ready_at: 0,
            created_at: 1,
            last_used_at,
        }
    }

    #[test]
    fn save_load_update_and_delete_round_trip_by_channel_id() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("channels.db");
        let conn = open_db_at(&db_path).unwrap();

        let channel_id = alloy::primitives::B256::from([0x11; 32]);
        let mut record = sample_record(channel_id, 5, ChannelStatus::Active);
        record.token = "0x00000000000000000000000000000000000000Aa".to_string();
        record.payee = "0x00000000000000000000000000000000000000Bb".to_string();
        record.server_spent = 37;
        save_channel_in_conn(&conn, &record).unwrap();

        let loaded = load_channel_in_conn(&conn, &format!("{channel_id:#x}"))
            .unwrap()
            .expect("channel should load");
        assert_eq!(loaded.channel_id, channel_id);
        assert_eq!(loaded.state, ChannelStatus::Active);
        // token/payee are canonicalized on load while payer remains raw.
        assert_eq!(loaded.token, "0x00000000000000000000000000000000000000aa");
        assert_eq!(loaded.payee, "0x00000000000000000000000000000000000000bb");
        assert_eq!(loaded.payer, "0x0000000000000000000000000000000000000004");
        assert_eq!(loaded.server_spent, 37);

        update_channel_close_state_in_conn(
            &conn,
            &format!("{channel_id:#x}"),
            ChannelStatus::Closing,
            42,
            99,
        )
        .unwrap();
        let updated = load_channel_in_conn(&conn, &format!("{channel_id:#x}"))
            .unwrap()
            .expect("updated channel should load");
        assert_eq!(updated.state, ChannelStatus::Closing);
        assert_eq!(updated.close_requested_at, 42);
        assert_eq!(updated.grace_ready_at, 99);

        delete_channel_in_conn(&conn, &format!("{channel_id:#x}")).unwrap();
        assert!(
            load_channel_in_conn(&conn, &format!("{channel_id:#x}"))
                .unwrap()
                .is_none(),
            "channel should be deleted"
        );
    }

    #[test]
    fn find_reusable_channel_prefers_most_recent_active_match() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("channels.db");
        let conn = open_db_at(&db_path).unwrap();

        let mut active_old = sample_record(
            alloy::primitives::B256::from([0x21; 32]),
            10,
            ChannelStatus::Active,
        );
        active_old.token = "0x0000000000000000000000000000000000000002".to_string();
        active_old.payee = "0x0000000000000000000000000000000000000003".to_string();

        let mut active_new = sample_record(
            alloy::primitives::B256::from([0x22; 32]),
            20,
            ChannelStatus::Active,
        );
        active_new.token = "0x0000000000000000000000000000000000000002".to_string();
        active_new.payee = "0x0000000000000000000000000000000000000003".to_string();

        let closing = sample_record(
            alloy::primitives::B256::from([0x23; 32]),
            30,
            ChannelStatus::Closing,
        );

        save_channel_in_conn(&conn, &active_old).unwrap();
        save_channel_in_conn(&conn, &active_new).unwrap();
        save_channel_in_conn(&conn, &closing).unwrap();

        let found = find_reusable_channel_in_conn(
            &conn,
            "https://api.example.com",
            "0x0000000000000000000000000000000000000004",
            "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            "0x0000000000000000000000000000000000000002",
            "0x0000000000000000000000000000000000000003",
            4217,
        )
        .unwrap()
        .expect("should find reusable active channel");

        assert_eq!(found.channel_id, alloy::primitives::B256::from([0x22; 32]));
        assert_eq!(found.state, ChannelStatus::Active);
    }

    #[test]
    fn find_reusable_channel_returns_none_on_identity_mismatch() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("channels.db");
        let conn = open_db_at(&db_path).unwrap();

        let mut record = sample_record(
            alloy::primitives::B256::from([0x31; 32]),
            1,
            ChannelStatus::Active,
        );
        record.token = "0x0000000000000000000000000000000000000002".to_string();
        record.payee = "0x0000000000000000000000000000000000000003".to_string();
        save_channel_in_conn(&conn, &record).unwrap();

        let found = find_reusable_channel_in_conn(
            &conn,
            "https://api.example.com",
            "0x0000000000000000000000000000000000000004",
            "0x0000000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            "0x00000000000000000000000000000000000000ff",
            "0x0000000000000000000000000000000000000003",
            4217,
        )
        .unwrap();

        assert!(found.is_none());
    }

    #[test]
    fn cumulative_floor_update_is_monotonic() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("channels.db");
        let conn = open_db_at(&db_path).unwrap();

        let channel_id = alloy::primitives::B256::from([0x32; 32]);
        let record = sample_record(channel_id, 1, ChannelStatus::Active);
        save_channel_in_conn(&conn, &record).unwrap();

        update_channel_cumulative_floor_in_conn(&conn, &format!("{channel_id:#x}"), 50).unwrap();
        update_channel_cumulative_floor_in_conn(&conn, &format!("{channel_id:#x}"), 12).unwrap();
        update_channel_cumulative_floor_in_conn(&conn, &format!("{channel_id:#x}"), 9_999).unwrap();

        let loaded = load_channel_in_conn(&conn, &format!("{channel_id:#x}"))
            .unwrap()
            .expect("channel should load");
        assert_eq!(loaded.cumulative_amount_u128(), 9_999);
    }

    #[test]
    fn stale_save_does_not_regress_cumulative_after_floor_update() {
        let temp = tempdir().unwrap();
        let db_path = temp.path().join("channels.db");
        let conn = open_db_at(&db_path).unwrap();

        let channel_id = alloy::primitives::B256::from([0x33; 32]);
        let mut initial = sample_record(channel_id, 1, ChannelStatus::Active);
        initial.cumulative_amount = 100;
        save_channel_in_conn(&conn, &initial).unwrap();

        update_channel_cumulative_floor_in_conn(&conn, &format!("{channel_id:#x}"), 150).unwrap();

        let mut stale = initial;
        stale.cumulative_amount = 120;
        stale.last_used_at = 2;
        save_channel_in_conn(&conn, &stale).unwrap();

        let loaded = load_channel_in_conn(&conn, &format!("{channel_id:#x}"))
            .unwrap()
            .expect("channel should load");
        assert_eq!(loaded.cumulative_amount_u128(), 150);
    }

    #[test]
    fn load_channels_by_origin_returns_all_matching_records_ordered() {
        let temp = tempdir().unwrap();
        std::env::set_var("TEMPO_HOME", temp.path());

        let first = sample_record(
            alloy::primitives::B256::from([0x41; 32]),
            10,
            ChannelStatus::Active,
        );
        let second = sample_record(
            alloy::primitives::B256::from([0x42; 32]),
            20,
            ChannelStatus::Closing,
        );

        save_channel(&first).unwrap();
        save_channel(&second).unwrap();
        let rows = load_channels_by_origin("https://api.example.com").unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].channel_id,
            alloy::primitives::B256::from([0x42; 32]),
            "most recently used channel should appear first"
        );
        assert_eq!(
            rows[1].channel_id,
            alloy::primitives::B256::from([0x41; 32])
        );
    }
}
