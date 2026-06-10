//! Service directory data model and formatting helpers.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub(super) const SERVICES_API_URL: &str = "https://mpp.sh/api/services";

#[derive(Deserialize)]
pub(super) struct ServiceRegistry {
    pub(super) services: Vec<Service>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Service {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) url: String,
    #[serde(default, rename = "serviceUrl")]
    pub(super) service_url: Option<String>,
    #[serde(default, rename = "supportsCredits")]
    pub(super) supports_credits: bool,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) icon: Option<String>,
    #[serde(default)]
    pub(super) categories: Vec<String>,
    #[serde(default)]
    pub(super) integration: Option<String>,
    #[serde(default)]
    pub(super) tags: Vec<String>,
    #[serde(default)]
    pub(super) status: Option<String>,
    #[serde(default)]
    pub(super) docs: Option<ServiceDocs>,
    #[serde(default)]
    pub(super) methods: HashMap<String, PaymentMethod>,
    #[serde(default)]
    pub(super) realm: Option<String>,
    #[serde(default)]
    pub(super) endpoints: Vec<Endpoint>,
    #[serde(default)]
    pub(super) provider: Option<Provider>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct ServiceDocs {
    #[serde(default)]
    pub(super) homepage: Option<String>,
    #[serde(default, rename = "llmsTxt")]
    pub(super) llms_txt: Option<String>,
    #[serde(default)]
    pub(super) openapi: Option<String>,
    #[serde(default, rename = "apiReference")]
    pub(super) api_reference: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct PaymentMethod {
    #[serde(default)]
    pub(super) intents: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Endpoint {
    pub(super) method: String,
    pub(super) path: String,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) payment: Option<EndpointPayment>,
    #[serde(default)]
    pub(super) docs: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EndpointMethod<'a> {
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Other(&'a str),
}

impl<'a> EndpointMethod<'a> {
    const fn parse(value: &'a str) -> Self {
        if value.eq_ignore_ascii_case("GET") {
            return Self::Get;
        }
        if value.eq_ignore_ascii_case("POST") {
            return Self::Post;
        }
        if value.eq_ignore_ascii_case("PUT") {
            return Self::Put;
        }
        if value.eq_ignore_ascii_case("PATCH") {
            return Self::Patch;
        }
        if value.eq_ignore_ascii_case("DELETE") {
            return Self::Delete;
        }
        if value.eq_ignore_ascii_case("HEAD") {
            return Self::Head;
        }
        if value.eq_ignore_ascii_case("OPTIONS") {
            return Self::Options;
        }
        Self::Other(value)
    }

    pub(super) const fn as_str(self) -> &'a str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
            Self::Other(value) => value,
        }
    }

    pub(super) const fn supports_body(self) -> bool {
        !matches!(self, Self::Get | Self::Head)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct EndpointPayment {
    pub(super) intent: String,
    #[serde(default)]
    pub(super) amount: Option<String>,
    #[serde(default)]
    pub(super) decimals: Option<u32>,
    #[serde(default, rename = "unitType")]
    pub(super) unit_type: Option<String>,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) dynamic: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Provider {
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) url: Option<String>,
    #[serde(default)]
    pub(super) icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceId(String);

impl ServiceId {
    fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        (!normalized.is_empty()).then_some(Self(normalized))
    }
}

impl ServiceRegistry {
    pub(super) fn find(&self, id: &str) -> Option<&Service> {
        let target = ServiceId::parse(id)?;
        self.services
            .iter()
            .find(|s| ServiceId::parse(&s.id).as_ref() == Some(&target))
    }
}

impl Service {
    pub(super) fn format_categories(&self) -> String {
        if self.categories.is_empty() {
            "—".to_string()
        } else {
            self.categories.join(", ")
        }
    }
}

impl Endpoint {
    pub(super) fn method_kind(&self) -> EndpointMethod<'_> {
        EndpointMethod::parse(&self.method)
    }

    pub(super) fn format_pricing(&self) -> String {
        match &self.payment {
            None => "free".to_string(),
            Some(p) => {
                let mut parts = Vec::new();
                if p.dynamic == Some(true) {
                    parts.push("dynamic".to_string());
                } else if let Some(amount) = &p.amount {
                    parts.push(format_amount(amount, p.decimals));
                }
                parts.push(p.intent.clone());
                parts.join(" ")
            }
        }
    }
}

/// Format a token amount string with the given decimal places as a dollar value.
pub(super) fn format_amount(amount: &str, decimals: Option<u32>) -> String {
    match decimals {
        Some(dec) if dec > 0 => match amount.parse::<u128>() {
            Ok(v) => {
                let dec = dec as usize;
                let padded = format!("{:0>width$}", v, width = dec + 1);
                let (int, frac) = padded.split_at(padded.len() - dec);
                format!("${int}.{frac}")
            }
            Err(_) => amount.to_string(),
        },
        _ => amount.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_amount_with_decimals() {
        assert_eq!(format_amount("1000000", Some(6)), "$1.000000");
        assert_eq!(format_amount("500", Some(2)), "$5.00");
    }

    #[test]
    fn format_amount_zero_decimals() {
        assert_eq!(format_amount("42", Some(0)), "42");
        assert_eq!(format_amount("42", None), "42");
    }

    #[test]
    fn format_amount_unparseable() {
        assert_eq!(format_amount("not-a-number", Some(6)), "not-a-number");
    }

    #[test]
    fn format_amount_small_value_pads() {
        // 123 with 6 decimals → $0.000123
        assert_eq!(format_amount("123", Some(6)), "$0.000123");
    }

    #[test]
    fn format_pricing_free() {
        let ep = Endpoint {
            method: "GET".into(),
            path: "/v1/test".into(),
            description: None,
            payment: None,
            docs: None,
        };
        assert_eq!(ep.format_pricing(), "free");
    }

    #[test]
    fn format_pricing_dynamic() {
        let ep = Endpoint {
            method: "POST".into(),
            path: "/v1/test".into(),
            description: None,
            payment: Some(EndpointPayment {
                intent: "charge".into(),
                amount: None,
                decimals: None,
                unit_type: None,
                description: None,
                dynamic: Some(true),
            }),
            docs: None,
        };
        assert_eq!(ep.format_pricing(), "dynamic charge");
    }

    #[test]
    fn format_pricing_fixed_amount() {
        let ep = Endpoint {
            method: "POST".into(),
            path: "/v1/test".into(),
            description: None,
            payment: Some(EndpointPayment {
                intent: "session".into(),
                amount: Some("1000000".into()),
                decimals: Some(6),
                unit_type: None,
                description: None,
                dynamic: None,
            }),
            docs: None,
        };
        assert_eq!(ep.format_pricing(), "$1.000000 session");
    }

    #[test]
    fn service_find_normalizes_identifier() {
        let registry = ServiceRegistry {
            services: vec![Service {
                id: "MY-SERVICE".to_string(),
                name: "n".to_string(),
                url: "u".to_string(),
                service_url: None,
                supports_credits: false,
                description: None,
                icon: None,
                categories: Vec::new(),
                integration: None,
                tags: Vec::new(),
                status: None,
                docs: None,
                methods: HashMap::new(),
                realm: None,
                endpoints: Vec::new(),
                provider: None,
            }],
        };

        assert!(registry.find("my-service").is_some());
        assert!(registry.find("  my-service  ").is_some());
        assert!(registry.find("").is_none());
    }

    #[test]
    fn endpoint_method_is_normalized() {
        let endpoint = Endpoint {
            method: "post".into(),
            path: "/v1/test".into(),
            description: None,
            payment: None,
            docs: None,
        };

        assert_eq!(endpoint.method_kind().as_str(), "POST");
        assert!(endpoint.method_kind().supports_body());
    }

    #[test]
    fn endpoint_method_get_disables_body_examples() {
        let endpoint = Endpoint {
            method: "GET".into(),
            path: "/v1/test".into(),
            description: None,
            payment: None,
            docs: None,
        };

        assert!(!endpoint.method_kind().supports_body());
    }
}
