//! Rendering functions for the services command.

use serde::Serialize;

use tempo_common::{
    cli::{
        output,
        output::OutputFormat,
        terminal::{print_field, sanitize_for_terminal, truncate},
    },
    error::TempoError,
};

use super::model::{EndpointPayment, Service, ServiceDocs};

// ── List serialization structs ───────────────────────────────────────

#[derive(Serialize)]
struct ServiceListItem<'a> {
    id: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_url: Option<&'a str>,
    #[serde(rename = "supportsCredits")]
    supports_credits: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    categories: Vec<&'a str>,
    tags: Vec<&'a str>,
    endpoint_count: usize,
}

// ── Detail serialization structs ─────────────────────────────────────

#[derive(Serialize)]
struct ServiceDetail<'a> {
    id: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    service_url: Option<&'a str>,
    #[serde(rename = "supportsCredits")]
    supports_credits: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    categories: Vec<&'a str>,
    tags: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    docs: Option<&'a ServiceDocs>,
    endpoints: Vec<EndpointDetailItem<'a>>,
}

#[derive(Serialize)]
struct EndpointDetailItem<'a> {
    method: &'a str,
    path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payment: Option<&'a EndpointPayment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    docs: Option<&'a str>,
}

// ── Public rendering entry points ────────────────────────────────────

pub(super) fn render_service_list(
    services: &[Service],
    output_format: OutputFormat,
    search: Option<&str>,
) -> Result<(), TempoError> {
    let search_query = search.map(|q| q.trim().to_ascii_lowercase());
    let filtered: Vec<&Service> = services
        .iter()
        .filter(|s| {
            if let Some(q_lower) = search_query.as_ref() {
                let matches = contains_case_insensitive(&s.name, q_lower)
                    || contains_case_insensitive(&s.id, q_lower)
                    || s.description
                        .as_ref()
                        .is_some_and(|d| contains_case_insensitive(d, q_lower))
                    || s.tags.iter().any(|t| contains_case_insensitive(t, q_lower))
                    || s.categories
                        .iter()
                        .any(|c| contains_case_insensitive(c, q_lower))
                    || (s.supports_credits && contains_case_insensitive("credits", q_lower));
                if !matches {
                    return false;
                }
            }
            true
        })
        .collect();

    let list_items: Vec<ServiceListItem<'_>> = filtered
        .iter()
        .map(|s| ServiceListItem {
            id: &s.id,
            name: &s.name,
            url: Some(&s.url),
            service_url: s.service_url.as_deref(),
            supports_credits: s.supports_credits,
            description: s.description.as_deref(),
            categories: s
                .categories
                .iter()
                .map(std::string::String::as_str)
                .collect(),
            tags: s.tags.iter().map(std::string::String::as_str).collect(),
            endpoint_count: s.endpoints.len(),
        })
        .collect();

    output::emit_by_format(output_format, &list_items, || {
        if filtered.is_empty() {
            println!("No services found.");
            return Ok(());
        }
        render_table(&filtered);
        Ok(())
    })?;

    Ok(())
}

fn contains_case_insensitive(haystack: &str, needle_lower: &str) -> bool {
    haystack.to_ascii_lowercase().contains(needle_lower)
}

pub(super) fn render_service_detail(
    service: &Service,
    output_format: OutputFormat,
) -> Result<(), TempoError> {
    let detail = ServiceDetail {
        id: &service.id,
        name: &service.name,
        url: Some(&service.url),
        service_url: service.service_url.as_deref(),
        supports_credits: service.supports_credits,
        description: service.description.as_deref(),
        categories: service
            .categories
            .iter()
            .map(std::string::String::as_str)
            .collect(),
        tags: service
            .tags
            .iter()
            .map(std::string::String::as_str)
            .collect(),
        docs: service.docs.as_ref(),
        endpoints: service
            .endpoints
            .iter()
            .map(|ep| EndpointDetailItem {
                method: ep.method_kind().as_str(),
                path: &ep.path,
                description: ep.description.as_deref(),
                payment: ep.payment.as_ref(),
                docs: ep.docs.as_deref(),
            })
            .collect(),
    };

    output::emit_by_format(output_format, &detail, || {
        render_detail(service);
        Ok(())
    })?;
    Ok(())
}

// ── Private rendering helpers ────────────────────────────────────────

fn render_table(services: &[&Service]) {
    const MAX_ID: usize = 20;
    const MAX_NAME: usize = 24;
    const MAX_CAT: usize = 16;

    let w_id = services
        .iter()
        .map(|s| s.id.len())
        .max()
        .unwrap_or(2)
        .clamp(2, MAX_ID);
    let w_name = services
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(4)
        .clamp(4, MAX_NAME);
    let w_cat = services
        .iter()
        .map(|s| s.format_categories().len())
        .max()
        .unwrap_or(8)
        .clamp(8, MAX_CAT);

    println!(
        "  {:<w_id$}  {:<w_name$}  {:<w_cat$}  Service URL",
        "ID", "Name", "Category"
    );
    let total_w = 2 + w_id + 2 + w_name + 2 + w_cat + 2 + 30;
    println!("  {}", "─".repeat(total_w));

    for s in services {
        let id = truncate(&s.id, MAX_ID);
        let name = truncate(&s.name, MAX_NAME);
        let categories = truncate(&s.format_categories(), MAX_CAT);
        let service_url = sanitize_for_terminal(s.service_url.as_deref().unwrap_or("—"));

        println!("  {id:<w_id$}  {name:<w_name$}  {categories:<w_cat$}  {service_url}");
    }

    println!("\n{} service(s).", services.len());
}

fn render_detail(s: &Service) {
    let safe_name = sanitize_for_terminal(&s.name);
    println!("{safe_name}");
    println!("{}", "─".repeat(safe_name.chars().count()));

    if let Some(desc) = &s.description {
        println!("{}", sanitize_for_terminal(desc));
    }
    println!();

    print_field("ID", &s.id);
    print_field("Categories", &s.format_categories());
    print_field("Service URL", s.service_url.as_deref().unwrap_or("—"));
    print_field("Upstream URL", &s.url);
    print_field(
        "Supports Credits",
        if s.supports_credits { "yes" } else { "no" },
    );

    if !s.tags.is_empty() {
        print_field("Tags", &s.tags.join(", "));
    }

    if let Some(docs) = &s.docs {
        let has_any = docs.homepage.is_some()
            || docs.llms_txt.is_some()
            || docs.openapi.is_some()
            || docs.api_reference.is_some();
        if has_any {
            println!();
            println!("Docs:");
            if let Some(v) = &docs.homepage {
                print_field("  Homepage", v);
            }
            if let Some(v) = &docs.llms_txt {
                print_field("  LLMs.txt", v);
            }
            if let Some(v) = &docs.openapi {
                print_field("  OpenAPI", v);
            }
            if let Some(v) = &docs.api_reference {
                print_field("  API Reference", v);
            }
        }
    }

    if !s.endpoints.is_empty() {
        println!();
        println!("Endpoints:");
        let base_url = s.service_url.as_deref().unwrap_or(&s.url);
        for ep in &s.endpoints {
            let pricing = sanitize_for_terminal(&ep.format_pricing());
            let method_kind = ep.method_kind();
            let method = sanitize_for_terminal(method_kind.as_str());
            let path = sanitize_for_terminal(&ep.path);
            println!("  {method:>6} {path:<40} {pricing}");

            let desc = ep
                .description
                .as_deref()
                .or_else(|| ep.payment.as_ref().and_then(|p| p.description.as_deref()));
            if let Some(desc) = desc {
                println!("         {}", sanitize_for_terminal(desc));
            }

            if let Some(unit_type) = ep.payment.as_ref().and_then(|p| p.unit_type.as_deref()) {
                println!("         per {}", sanitize_for_terminal(unit_type));
            }

            let full_url = format!("{}{}", base_url.trim_end_matches('/'), ep.path);
            let safe_url = sanitize_for_terminal(&full_url);
            let example = if method_kind.supports_body() {
                format!(
                    "tempo request -X {} --json '{{}}' {safe_url}",
                    method_kind.as_str()
                )
            } else {
                format!("tempo request {safe_url}")
            };
            println!("         example: {example}");

            if let Some(docs_url) = &ep.docs {
                println!("         docs: {}", sanitize_for_terminal(docs_url));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::commands::services::model::{Endpoint, PaymentMethod, Provider};

    fn sample_service() -> Service {
        Service {
            id: "weather-pro".to_string(),
            name: "Weather Pro".to_string(),
            url: "https://api.weather.example".to_string(),
            service_url: Some("https://mpp.weather.example".to_string()),
            supports_credits: true,
            description: Some("Premium weather data".to_string()),
            icon: None,
            categories: vec!["weather".to_string(), "data".to_string()],
            integration: None,
            tags: vec!["forecast".to_string(), "radar".to_string()],
            status: None,
            docs: Some(ServiceDocs {
                homepage: Some("https://docs.weather.example".to_string()),
                llms_txt: None,
                openapi: Some("https://docs.weather.example/openapi.json".to_string()),
                api_reference: None,
            }),
            methods: HashMap::<String, PaymentMethod>::new(),
            realm: None,
            endpoints: vec![
                Endpoint {
                    method: "GET".to_string(),
                    path: "/v1/forecast".to_string(),
                    description: Some("Forecast endpoint".to_string()),
                    payment: None,
                    docs: Some("https://docs.weather.example/forecast".to_string()),
                },
                Endpoint {
                    method: "POST".to_string(),
                    path: "/v1/alerts".to_string(),
                    description: Some("Create alert".to_string()),
                    payment: None,
                    docs: None,
                },
            ],
            provider: Some(Provider {
                name: Some("Weather Inc".to_string()),
                url: None,
                icon: None,
            }),
        }
    }

    #[test]
    fn service_list_item_schema_is_summary_only() {
        let service = sample_service();

        let item = ServiceListItem {
            id: &service.id,
            name: &service.name,
            url: Some(&service.url),
            service_url: service.service_url.as_deref(),
            supports_credits: service.supports_credits,
            description: service.description.as_deref(),
            categories: service
                .categories
                .iter()
                .map(std::string::String::as_str)
                .collect(),
            tags: service
                .tags
                .iter()
                .map(std::string::String::as_str)
                .collect(),
            endpoint_count: service.endpoints.len(),
        };

        let value = serde_json::to_value(item).expect("list item should serialize");
        let object = value
            .as_object()
            .expect("serialized list item should be an object");

        assert!(object.contains_key("id"));
        assert!(object.contains_key("name"));
        assert!(object.contains_key("url"));
        assert!(object.contains_key("service_url"));
        assert!(object.contains_key("description"));
        assert!(object.contains_key("categories"));
        assert!(object.contains_key("tags"));
        assert_eq!(object.get("endpoint_count"), Some(&serde_json::json!(2)));

        assert!(object.get("docs").is_none());
        assert!(object.get("endpoints").is_none());
    }
}
