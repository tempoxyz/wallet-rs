//! HTTP response type.

use tempo_common::error::{NetworkError, TempoError};

type HttpResult<T> = std::result::Result<T, TempoError>;

fn normalize_header_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

#[derive(Debug)]
pub(crate) struct HttpResponse {
    pub(crate) status_code: u16,
    /// Response headers stored as name-value pairs with **lowercased** names.
    ///
    /// Header names are normalized to lowercase during conversion.
    /// Duplicate header names (e.g., multiple `Set-Cookie`) are preserved.
    /// Use [`header`](Self::header) for lookup by lowercase name (returns the last value).
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
    /// The final URL after following any redirects.
    pub(crate) final_url: Option<String>,
}

/// Extract response headers as lowercased name-value pairs, skipping non-UTF-8 values.
pub(crate) fn headers_from_reqwest(headers: &reqwest::header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| {
            v.to_str()
                .ok()
                .map(|s| (normalize_header_name(k.as_str()), s.to_string()))
        })
        .collect()
}

impl HttpResponse {
    /// Convert a reqwest response into an `HttpResponse`.
    pub(crate) async fn from_reqwest(mut response: reqwest::Response) -> HttpResult<Self> {
        let status_code = response.status().as_u16();
        let final_url = Some(response.url().to_string());
        let headers = headers_from_reqwest(response.headers());
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(NetworkError::Reqwest)? {
            body.extend_from_slice(&chunk);
        }

        Ok(Self {
            status_code,
            headers,
            body,
            final_url,
        })
    }

    /// Convert the response body to a UTF-8 string.
    ///
    /// # Errors
    /// Returns an error if the body is not valid UTF-8.
    pub(crate) fn body_string(&self) -> Result<String, TempoError> {
        let text = std::str::from_utf8(&self.body).map_err(|source| {
            NetworkError::ResponseSchemaSource {
                context: "HTTP response body",
                source: Box::new(source),
            }
        })?;
        Ok(text.to_string())
    }

    /// Look up a header value by name.
    ///
    /// Header lookup is case-insensitive. Returns the last value if the header
    /// appears multiple times (e.g. multiple `Set-Cookie` lines); use
    /// [`headers_all`](Self::headers_all) to retrieve every value.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        let name = normalize_header_name(name);
        self.headers
            .iter()
            .rev()
            .find(|(k, _)| k == &name)
            .map(|(_, v)| v.as_str())
    }

    /// Return every value for a header name, in the order they were received.
    ///
    /// Header lookup is case-insensitive. Use this for headers that may
    /// legitimately repeat per RFC 9110 §5.3, e.g. `WWW-Authenticate`.
    pub(crate) fn headers_all(&self, name: &str) -> Vec<&str> {
        let name = normalize_header_name(name);
        self.headers
            .iter()
            .filter(|(k, _)| k == &name)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

impl HttpResponse {
    /// Create a test response with the given status and body.
    #[cfg(test)]
    pub(crate) fn for_test(status: u16, body: &[u8]) -> Self {
        Self {
            status_code: status,
            headers: Vec::new(),
            body: body.to_vec(),
            final_url: None,
        }
    }

    /// Create a test response with the given status, body, and headers.
    #[cfg(test)]
    pub(crate) fn for_test_with_headers(
        status: u16,
        body: &[u8],
        headers: &[(&str, &str)],
    ) -> Self {
        Self {
            status_code: status,
            headers: headers
                .iter()
                .map(|(name, value)| (normalize_header_name(name), (*value).to_string()))
                .collect(),
            body: body.to_vec(),
            final_url: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_string_valid_utf8() {
        let resp = HttpResponse::for_test(200, b"hello world");
        assert_eq!(resp.body_string().unwrap(), "hello world");
    }

    #[test]
    fn body_string_invalid_utf8() {
        let resp = HttpResponse::for_test(200, &[0xff, 0xfe]);
        assert!(resp.body_string().is_err());
    }

    #[test]
    fn header_returns_value_for_matching_key() {
        let resp = HttpResponse {
            status_code: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Vec::new(),
            final_url: None,
        };
        assert_eq!(resp.header("content-type"), Some("application/json"));
    }

    #[test]
    fn header_returns_none_for_missing_key() {
        let resp = HttpResponse::for_test(200, b"");
        assert_eq!(resp.header("x-missing"), None);
    }

    #[test]
    fn header_returns_last_value_for_duplicates() {
        let resp = HttpResponse {
            status_code: 200,
            headers: vec![
                ("set-cookie".into(), "a=1".into()),
                ("set-cookie".into(), "b=2".into()),
            ],
            body: Vec::new(),
            final_url: None,
        };
        assert_eq!(resp.header("set-cookie"), Some("b=2"));
    }

    #[test]
    fn headers_all_returns_every_value_in_order() {
        let resp = HttpResponse {
            status_code: 401,
            headers: vec![
                ("www-authenticate".into(), "Payment id=\"a\"".into()),
                ("content-type".into(), "text/plain".into()),
                ("www-authenticate".into(), "Payment id=\"b\"".into()),
            ],
            body: Vec::new(),
            final_url: None,
        };
        assert_eq!(
            resp.headers_all("WWW-Authenticate"),
            vec!["Payment id=\"a\"", "Payment id=\"b\""],
        );
    }

    #[test]
    fn headers_all_empty_for_missing_key() {
        let resp = HttpResponse::for_test(200, b"");
        assert!(resp.headers_all("x-missing").is_empty());
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let resp = HttpResponse {
            status_code: 200,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: Vec::new(),
            final_url: None,
        };
        assert_eq!(resp.header("Content-Type"), Some("text/plain"));
    }
}
