use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use hmac::{Hmac, Mac};
use quick_xml::{Reader, events::Event as XmlEvent};
use reqwest::{Client, Method, Response, StatusCode};
use sha2::Sha256;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MGMT_API_VERSION: &str = "2021-05";
const RUNTIME_API_VERSION: &str = "2014-01";
const PAGE_SIZE: usize = 100;

// ---------------------------------------------------------------------------
// Connection string & SAS
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub host: String,
    pub key_name: String,
    pub key: String,
}

impl ConnectionInfo {
    pub fn parse(cs: &str) -> Result<Self> {
        let (mut endpoint, mut key_name, mut key) = (None, None, None);
        for part in cs.trim().split(';') {
            let Some((k, v)) = part.trim().split_once('=') else {
                continue;
            };
            let v = v.trim().to_string();
            match k.trim().to_ascii_lowercase().as_str() {
                "endpoint" => endpoint = Some(v),
                "sharedaccesskeyname" => key_name = Some(v),
                "sharedaccesskey" => key = Some(v),
                _ => {}
            }
        }
        let endpoint = endpoint.context("Missing endpoint in connection string")?;
        let host = endpoint
            .trim_start_matches("sb://")
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string();
        if host.is_empty() {
            bail!("Invalid endpoint");
        }
        Ok(Self {
            host,
            key_name: key_name.context("Missing SharedAccessKeyName")?,
            key: key.context("Missing SharedAccessKey")?,
        })
    }

    /// SAS token scoped to the entire namespace (valid for all entities).
    pub fn sas_token(&self, ttl_secs: u64) -> Result<String> {
        let uri = format!("https://{}/", self.host).to_lowercase();
        let encoded_uri = urlencoding::encode(&uri).into_owned();
        let expiry = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + ttl_secs;
        let mut mac = Hmac::<Sha256>::new_from_slice(self.key.as_bytes())
            .map_err(|e| anyhow!("Invalid SAS key: {e}"))?;
        mac.update(format!("{encoded_uri}\n{expiry}").as_bytes());
        let sig = STANDARD.encode(mac.finalize().into_bytes());
        Ok(format!(
            "SharedAccessSignature sr={encoded_uri}&sig={}&se={expiry}&skn={}",
            urlencoding::encode(&sig),
            urlencoding::encode(&self.key_name)
        ))
    }
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SubscriptionInfo {
    pub name: String,
    pub active_count: u64,
    pub dead_letter_count: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubscriptionOptions {
    pub lock_duration: Option<String>,
    pub requires_session: bool,
    pub default_message_ttl: Option<String>,
    pub dead_lettering_on_message_expiration: bool,
    pub dead_lettering_on_filter_evaluation_exceptions: Option<bool>,
    pub max_delivery_count: Option<u32>,
    pub enable_batched_operations: Option<bool>,
    pub status: Option<String>,
    pub forward_to: Option<String>,
    pub user_metadata: Option<String>,
    pub forward_dead_lettered_messages_to: Option<String>,
    pub auto_delete_on_idle: Option<String>,
}

/// Validate a subscription name against Azure Service Bus naming rules.
pub fn validate_subscription_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Subscription name is required");
    }
    if name.len() > 50 {
        bail!("Subscription name cannot exceed 50 characters");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        bail!("Subscription name can only contain letters, numbers, '.', '-', and '_'");
    }
    Ok(())
}

/// Build the AtomPub XML body for `PUT /{topic}/Subscriptions/{name}`.
/// Element order matches the Azure Service Bus schema.
pub fn build_subscription_xml(opts: &SubscriptionOptions) -> String {
    let mut inner = String::new();
    if let Some(d) = &opts.lock_duration {
        inner.push_str(&format!("<LockDuration>{}</LockDuration>", xml_escape(d)));
    }
    inner.push_str(&format!(
        "<RequiresSession>{}</RequiresSession>",
        opts.requires_session
    ));
    if let Some(ttl) = &opts.default_message_ttl {
        inner.push_str(&format!(
            "<DefaultMessageTimeToLive>{}</DefaultMessageTimeToLive>",
            xml_escape(ttl)
        ));
    }
    inner.push_str(&format!(
        "<DeadLetteringOnMessageExpiration>{}</DeadLetteringOnMessageExpiration>",
        opts.dead_lettering_on_message_expiration
    ));
    if let Some(v) = opts.dead_lettering_on_filter_evaluation_exceptions {
        inner.push_str(&format!(
            "<DeadLetteringOnFilterEvaluationExceptions>{v}</DeadLetteringOnFilterEvaluationExceptions>"
        ));
    }
    if let Some(max) = opts.max_delivery_count {
        inner.push_str(&format!("<MaxDeliveryCount>{max}</MaxDeliveryCount>"));
    }
    if let Some(v) = opts.enable_batched_operations {
        inner.push_str(&format!(
            "<EnableBatchedOperations>{v}</EnableBatchedOperations>"
        ));
    }
    if let Some(s) = &opts.status {
        inner.push_str(&format!("<Status>{}</Status>", xml_escape(s)));
    }
    if let Some(f) = &opts.forward_to {
        inner.push_str(&format!("<ForwardTo>{}</ForwardTo>", xml_escape(f)));
    }
    if let Some(m) = &opts.user_metadata {
        inner.push_str(&format!("<UserMetadata>{}</UserMetadata>", xml_escape(m)));
    }
    if let Some(f) = &opts.forward_dead_lettered_messages_to {
        inner.push_str(&format!(
            "<ForwardDeadLetteredMessagesTo>{}</ForwardDeadLetteredMessagesTo>",
            xml_escape(f)
        ));
    }
    if let Some(a) = &opts.auto_delete_on_idle {
        inner.push_str(&format!(
            "<AutoDeleteOnIdle>{}</AutoDeleteOnIdle>",
            xml_escape(a)
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><entry xmlns="http://www.w3.org/2005/Atom"><content type="application/xml"><SubscriptionDescription xmlns="http://schemas.microsoft.com/netservices/2010/10/servicebus/connect" xmlns:i="http://www.w3.org/2001/XMLSchema-instance">{inner}</SubscriptionDescription></content></entry>"#
    )
}

/// Parse a single AtomPub `<entry>` response from `GET /{topic}/Subscriptions/{name}`.
pub fn parse_subscription_options(xml: &str) -> Result<SubscriptionOptions> {
    let entries = parse_feed(xml)?;
    let entry = entries
        .into_iter()
        .next()
        .context("Missing subscription entry in response")?;
    let opt_str = |k: &str| entry.get(k).filter(|v| !v.is_empty()).cloned();
    let opt_bool = |k: &str| entry.get(k).and_then(|v| v.parse::<bool>().ok());
    Ok(SubscriptionOptions {
        lock_duration: opt_str("LockDuration"),
        requires_session: opt_bool("RequiresSession").unwrap_or(false),
        default_message_ttl: opt_str("DefaultMessageTimeToLive"),
        dead_lettering_on_message_expiration: opt_bool("DeadLetteringOnMessageExpiration")
            .unwrap_or(false),
        dead_lettering_on_filter_evaluation_exceptions: opt_bool(
            "DeadLetteringOnFilterEvaluationExceptions",
        ),
        max_delivery_count: entry.get("MaxDeliveryCount").and_then(|v| v.parse().ok()),
        enable_batched_operations: opt_bool("EnableBatchedOperations"),
        status: opt_str("Status"),
        forward_to: opt_str("ForwardTo"),
        user_metadata: opt_str("UserMetadata"),
        forward_dead_lettered_messages_to: opt_str("ForwardDeadLetteredMessagesTo"),
        auto_delete_on_idle: opt_str("AutoDeleteOnIdle"),
    })
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[derive(Debug, Clone)]
pub struct Message {
    pub sequence_number: i64,
    pub message_id: String,
    pub enqueued: String,
    pub delivery_count: i64,
    pub label: Option<String>,
    pub content_type: Option<String>,
    pub broker_properties: Vec<(String, String)>,
    pub user_properties: Vec<(String, String)>,
    pub body: String,
}

/// Message obtained via peek-lock: `location` identifies the lock (unlock/delete).
#[derive(Debug, Clone)]
pub struct LockedMessage {
    pub message: Message,
    pub location: String,
}

pub fn entity_path(topic: &str, sub: &str, dlq: bool) -> String {
    if dlq {
        format!("{topic}/subscriptions/{sub}/$DeadLetterQueue")
    } else {
        format!("{topic}/subscriptions/{sub}")
    }
}

// ---------------------------------------------------------------------------
// Client REST
// ---------------------------------------------------------------------------

pub struct AzureClient {
    conn: ConnectionInfo,
    http: Client,
}

impl AzureClient {
    pub fn new(conn: ConnectionInfo) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self { conn, http })
    }

    pub fn host(&self) -> &str {
        &self.conn.host
    }

    async fn send(&self, method: Method, url: &str) -> Result<Response> {
        let token = self.conn.sas_token(3600)?;
        let mut req = self
            .http
            .request(method.clone(), url)
            .header("Authorization", token);
        if method == Method::GET {
            req = req.header(
                "Content-Type",
                "application/atom+xml;type=entry;charset=utf-8",
            );
        } else {
            // Azure (front-end IIS) risponde 411 se manca `Content-Length`:
            // Hyper omits it for empty bodies, so it must be set explicitly.
            req = req
                .header(reqwest::header::CONTENT_LENGTH, "0")
                .body(Vec::<u8>::new());
        }
        req.send()
            .await
            .with_context(|| format!("{method} request failed"))
    }

    async fn get_feed(&self, url: &str) -> Result<Vec<HashMap<String, String>>> {
        let resp = self.send(Method::GET, url).await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            bail!("HTTP {status}: {}", error_detail(&text));
        }
        parse_feed(&text)
    }

    pub async fn list_topics(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut skip = 0;
        loop {
            let url = format!(
                "https://{}/$Resources/Topics?api-version={MGMT_API_VERSION}&$skip={skip}&$top={PAGE_SIZE}",
                self.conn.host
            );
            let entries = self.get_feed(&url).await?;
            let n = entries.len();
            out.extend(entries.into_iter().filter_map(|mut e| e.remove("title")));
            if n < PAGE_SIZE {
                break;
            }
            skip += PAGE_SIZE;
        }
        out.sort_by_key(|s| s.to_lowercase());
        Ok(out)
    }

    pub async fn list_subscriptions(&self, topic: &str) -> Result<Vec<SubscriptionInfo>> {
        let mut out = Vec::new();
        let mut skip = 0;
        loop {
            let url = format!(
                "https://{}/{topic}/Subscriptions?api-version={MGMT_API_VERSION}&$skip={skip}&$top={PAGE_SIZE}",
                self.conn.host
            );
            let entries = self.get_feed(&url).await?;
            let n = entries.len();
            for e in entries {
                let Some(name) = e.get("title").cloned() else {
                    continue;
                };
                let num = |k: &str| e.get(k).and_then(|v| v.parse().ok()).unwrap_or(0);
                out.push(SubscriptionInfo {
                    name,
                    active_count: num("ActiveMessageCount"),
                    dead_letter_count: num("DeadLetterMessageCount"),
                });
            }
            if n < PAGE_SIZE {
                break;
            }
            skip += PAGE_SIZE;
        }
        out.sort_by_key(|s| s.name.to_lowercase());
        Ok(out)
    }

    pub async fn get_subscription(&self, topic: &str, name: &str) -> Result<SubscriptionOptions> {
        let url = format!(
            "https://{}/{topic}/Subscriptions/{name}?api-version={MGMT_API_VERSION}",
            self.conn.host
        );
        let resp = self.send(Method::GET, &url).await?;
        let status = resp.status();
        let text = resp.text().await?;
        if status == StatusCode::NOT_FOUND {
            bail!("Subscription '{name}' not found in topic '{topic}'");
        }
        if !status.is_success() {
            bail!(
                "Get subscription failed (HTTP {status}): {}",
                error_detail(&text)
            );
        }
        parse_subscription_options(&text)
    }

    pub async fn create_subscription(
        &self,
        topic: &str,
        name: &str,
        opts: &SubscriptionOptions,
    ) -> Result<()> {
        validate_subscription_name(name)?;
        self.put_subscription(topic, name, opts, false).await
    }

    pub async fn update_subscription(
        &self,
        topic: &str,
        name: &str,
        opts: &SubscriptionOptions,
    ) -> Result<()> {
        self.put_subscription(topic, name, opts, true).await
    }

    pub async fn delete_subscription(&self, topic: &str, name: &str) -> Result<()> {
        let url = format!(
            "https://{}/{topic}/Subscriptions/{name}?api-version={MGMT_API_VERSION}",
            self.conn.host
        );
        let resp = self.send(Method::DELETE, &url).await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let detail = error_detail(&text);
            if !detail.is_empty() {
                bail!("Delete subscription failed (HTTP {status}): {detail}");
            }
            if status == StatusCode::NOT_FOUND {
                bail!("Subscription '{name}' not found in topic '{topic}' (HTTP 404)");
            }
            bail!("Delete subscription failed (HTTP {status})");
        }
        Ok(())
    }

    async fn put_subscription(
        &self,
        topic: &str,
        name: &str,
        opts: &SubscriptionOptions,
        update: bool,
    ) -> Result<()> {
        let url = format!(
            "https://{}/{topic}/Subscriptions/{name}?api-version={MGMT_API_VERSION}",
            self.conn.host
        );
        let body = build_subscription_xml(opts);
        let token = self.conn.sas_token(3600)?;
        let mut req = self
            .http
            .put(&url)
            .header("Authorization", token)
            .header(
                "Content-Type",
                "application/atom+xml;type=entry;charset=utf-8",
            )
            .header(reqwest::header::CONTENT_LENGTH, body.len().to_string());
        if update {
            req = req.header("If-Match", "*");
        }
        let op = if update { "Update" } else { "Create" };
        let resp = req
            .body(body)
            .send()
            .await
            .with_context(|| format!("PUT {op} subscription request failed"))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let detail = error_detail(&text);
            if !detail.is_empty() {
                bail!("{op} subscription failed (HTTP {status}): {detail}");
            }
            if !update && status == StatusCode::CONFLICT {
                bail!("Subscription '{name}' already exists in topic '{topic}' (HTTP 409)");
            }
            if update && status == StatusCode::NOT_FOUND {
                bail!("Subscription '{name}' not found in topic '{topic}' (HTTP 404)");
            }
            bail!("{op} subscription failed (HTTP {status})");
        }
        Ok(())
    }

    /// Peek-lock the first unlocked message. Returns `None` if the queue is empty.
    pub async fn peek_lock(&self, path: &str) -> Result<Option<LockedMessage>> {
        let url = format!(
            "https://{}/{path}/messages/head?timeout=1&api-version={RUNTIME_API_VERSION}",
            self.conn.host
        );
        let resp = self.send(Method::POST, &url).await?;
        match resp.status() {
            StatusCode::NO_CONTENT => Ok(None),
            StatusCode::CREATED | StatusCode::OK => parse_message(resp).await.map(Some),
            s => bail!(
                "Peek failed (HTTP {s}): {}",
                error_detail(&resp.text().await.unwrap_or_default())
            ),
        }
    }

    /// Release the lock: the message becomes available in the queue again.
    pub async fn unlock(&self, location: &str) -> Result<()> {
        self.lock_op(Method::PUT, location).await
    }

    /// Complete (delete) a locked message.
    pub async fn delete_locked(&self, location: &str) -> Result<()> {
        self.lock_op(Method::DELETE, location).await
    }

    async fn lock_op(&self, method: Method, location: &str) -> Result<()> {
        let resp = self.send(method.clone(), location).await?;
        let s = resp.status();
        if !s.is_success() {
            bail!(
                "{method} failed (HTTP {s}): {}",
                error_detail(&resp.text().await.unwrap_or_default())
            );
        }
        Ok(())
    }

    /// Destructive AMQP batch purge of either the subscription or its DLQ.
    pub async fn purge(
        &self,
        topic: &str,
        sub: &str,
        dlq: bool,
        progress: impl FnMut(usize) + Send,
    ) -> Result<usize> {
        crate::purge::purge(&self.conn, topic, sub, dlq, progress).await
    }
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

const STANDARD_HEADERS: &[&str] = &[
    "brokerproperties",
    "content-type",
    "content-length",
    "location",
    "date",
    "server",
    "transfer-encoding",
    "strict-transport-security",
    "connection",
];

const HIDDEN_BROKER_PROPS: &[&str] = &["LockToken", "LockedUntilUtc"];

async fn parse_message(resp: Response) -> Result<LockedMessage> {
    let headers = resp.headers().clone();
    let location = headers
        .get("location")
        .and_then(|v| v.to_str().ok())
        .context("Missing Location header in response")?
        .to_string();
    let broker: serde_json::Map<String, serde_json::Value> = headers
        .get("brokerproperties")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    let str_prop = |k: &str| broker.get(k).map(json_to_string);
    let sequence_number = broker
        .get("SequenceNumber")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1);
    let enqueued = str_prop("EnqueuedTimeUtc")
        .map(|s| {
            chrono::DateTime::parse_from_rfc2822(&s)
                .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or(s)
        })
        .unwrap_or_default();

    let mut user_properties: Vec<(String, String)> = headers
        .iter()
        .filter(|(k, _)| !STANDARD_HEADERS.contains(&k.as_str()))
        .map(|(k, v)| {
            let v = String::from_utf8_lossy(v.as_bytes());
            (k.to_string(), v.trim_matches('"').to_string())
        })
        .collect();
    user_properties.sort();

    let broker_properties = broker
        .iter()
        .filter(|(k, _)| !HIDDEN_BROKER_PROPS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), json_to_string(v)))
        .collect();

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = String::from_utf8_lossy(&resp.bytes().await?).into_owned();

    Ok(LockedMessage {
        message: Message {
            sequence_number,
            message_id: str_prop("MessageId").unwrap_or_default(),
            enqueued,
            delivery_count: broker
                .get("DeliveryCount")
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            label: str_prop("Label"),
            content_type,
            broker_properties,
            user_properties,
            body,
        },
        location,
    })
}

fn json_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Generic Atom feed parser: returns a map of `local-name -> text`
/// (first occurrence) for each `<entry>`.
pub fn parse_feed(xml: &str) -> Result<Vec<HashMap<String, String>>> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut entries = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;
    let mut stack: Vec<String> = Vec::new();
    loop {
        match reader.read_event()? {
            XmlEvent::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                if name == "entry" {
                    current = Some(HashMap::new());
                }
                stack.push(name);
            }
            XmlEvent::End(_) => {
                if stack.pop().as_deref() == Some("entry")
                    && let Some(c) = current.take()
                {
                    entries.push(c);
                }
            }
            XmlEvent::Text(t) => {
                if let (Some(cur), Some(name)) = (current.as_mut(), stack.last()) {
                    let v = t.unescape()?.into_owned();
                    cur.entry(name.clone()).or_insert(v);
                }
            }
            XmlEvent::Eof => break,
            _ => {}
        }
    }
    Ok(entries)
}

/// Extract a readable message from an Azure error response (XML or HTML).
fn error_detail(body: &str) -> String {
    if let Some(start) = body.find("<Detail>")
        && let Some(end) = body[start..].find("</Detail>")
    {
        return collapse_ws(&body[start + 8..start + end]);
    }
    // For HTML pages (e.g. IIS errors), keep only the text inside <body>.
    let content = body
        .to_ascii_lowercase()
        .find("<body")
        .map_or(body, |i| &body[i..]);
    collapse_ws(&strip_tags(content))
        .chars()
        .take(300)
        .collect()
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_connection_string() {
        let c = ConnectionInfo::parse(
            "Endpoint=sb://ns.servicebus.windows.net/;SharedAccessKeyName=Root;SharedAccessKey=abc=",
        )
        .unwrap();
        assert_eq!(c.host, "ns.servicebus.windows.net");
        assert_eq!(c.key_name, "Root");
        assert_eq!(c.key, "abc=");
        assert!(
            c.sas_token(60)
                .unwrap()
                .starts_with("SharedAccessSignature sr=https%3A%2F%2Fns.")
        );
    }

    #[test]
    fn extracts_html_error() {
        let html = r#"<!DOCTYPE HTML PUBLIC "-//W3C//DTD HTML 4.01//EN"><HTML><HEAD><TITLE>Length Required</TITLE></HEAD>
<BODY><h2>Length Required</h2>
<hr><p>HTTP Error 411. The request must be chunked or have a content length.</p></BODY></HTML>"#;
        assert_eq!(
            error_detail(html),
            "Length Required HTTP Error 411. The request must be chunked or have a content length."
        );
        assert_eq!(
            error_detail("<Error><Code>401</Code><Detail>Unauthorized</Detail></Error>"),
            "Unauthorized"
        );
    }

    #[test]
    fn parses_subscription_feed() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title type="text">Subscriptions</title>
<entry><title type="text">worker</title><content type="application/xml">
<SubscriptionDescription xmlns="http://schemas.microsoft.com/netservices/2010/10/servicebus/connect">
<MessageCount>14</MessageCount><CountDetails xmlns:d2p1="http://schemas.microsoft.com/netservices/2011/06/servicebus">
<d2p1:ActiveMessageCount>2</d2p1:ActiveMessageCount><d2p1:DeadLetterMessageCount>12</d2p1:DeadLetterMessageCount>
</CountDetails></SubscriptionDescription></content></entry></feed>"#;
        let e = parse_feed(xml).unwrap();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0]["title"], "worker");
        assert_eq!(e[0]["MessageCount"], "14");
        assert_eq!(e[0]["ActiveMessageCount"], "2");
        assert_eq!(e[0]["DeadLetterMessageCount"], "12");
    }

    #[test]
    fn validates_subscription_name() {
        assert!(validate_subscription_name("my-sub_1.test").is_ok());
        assert!(validate_subscription_name("").is_err());
        assert!(validate_subscription_name(&"a".repeat(51)).is_err());
        assert!(validate_subscription_name(&"a".repeat(50)).is_ok());
        assert!(validate_subscription_name("bad/name").is_err());
        assert!(validate_subscription_name("bad name").is_err());
        assert!(validate_subscription_name("bad?name").is_err());
    }

    #[test]
    fn builds_subscription_xml_in_schema_order() {
        let xml = build_subscription_xml(&SubscriptionOptions {
            lock_duration: Some("PT1M".into()),
            requires_session: true,
            default_message_ttl: Some("P14D".into()),
            dead_lettering_on_message_expiration: true,
            dead_lettering_on_filter_evaluation_exceptions: Some(true),
            max_delivery_count: Some(5),
            enable_batched_operations: Some(true),
            status: Some("Active".into()),
            forward_to: Some("target-queue".into()),
            user_metadata: Some("meta".into()),
            forward_dead_lettered_messages_to: Some("dlq-queue".into()),
            auto_delete_on_idle: Some("P10675199DT2H48M5.4775807S".into()),
        });
        let positions = [
            xml.find("<LockDuration>PT1M</LockDuration>").unwrap(),
            xml.find("<RequiresSession>true</RequiresSession>").unwrap(),
            xml.find("<DefaultMessageTimeToLive>P14D</DefaultMessageTimeToLive>")
                .unwrap(),
            xml.find("<DeadLetteringOnMessageExpiration>true</DeadLetteringOnMessageExpiration>")
                .unwrap(),
            xml.find("<DeadLetteringOnFilterEvaluationExceptions>true</DeadLetteringOnFilterEvaluationExceptions>")
                .unwrap(),
            xml.find("<MaxDeliveryCount>5</MaxDeliveryCount>").unwrap(),
            xml.find("<EnableBatchedOperations>true</EnableBatchedOperations>")
                .unwrap(),
            xml.find("<Status>Active</Status>").unwrap(),
            xml.find("<ForwardTo>target-queue</ForwardTo>").unwrap(),
            xml.find("<UserMetadata>meta</UserMetadata>").unwrap(),
            xml.find("<ForwardDeadLetteredMessagesTo>dlq-queue</ForwardDeadLetteredMessagesTo>")
                .unwrap(),
            xml.find("<AutoDeleteOnIdle>P10675199DT2H48M5.4775807S</AutoDeleteOnIdle>")
                .unwrap(),
        ];
        assert!(positions.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn builds_minimal_subscription_xml() {
        let xml = build_subscription_xml(&SubscriptionOptions::default());
        assert!(!xml.contains("<LockDuration>"));
        assert!(!xml.contains("<DefaultMessageTimeToLive>"));
        assert!(!xml.contains("<MaxDeliveryCount>"));
        assert!(xml.contains("<RequiresSession>false</RequiresSession>"));
        assert!(
            xml.contains("<DeadLetteringOnMessageExpiration>false</DeadLetteringOnMessageExpiration>")
        );
    }

    #[test]
    fn parses_subscription_options_from_entry() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<entry xmlns="http://www.w3.org/2005/Atom">
  <title type="text">worker</title>
  <content type="application/xml">
    <SubscriptionDescription xmlns="http://schemas.microsoft.com/netservices/2010/10/servicebus/connect">
      <LockDuration>PT2M</LockDuration>
      <RequiresSession>true</RequiresSession>
      <DefaultMessageTimeToLive>P7D</DefaultMessageTimeToLive>
      <DeadLetteringOnMessageExpiration>true</DeadLetteringOnMessageExpiration>
      <DeadLetteringOnFilterEvaluationExceptions>false</DeadLetteringOnFilterEvaluationExceptions>
      <MaxDeliveryCount>15</MaxDeliveryCount>
      <EnableBatchedOperations>true</EnableBatchedOperations>
      <Status>Active</Status>
      <ForwardTo>dest</ForwardTo>
      <UserMetadata>team-a</UserMetadata>
      <AutoDeleteOnIdle>PT1H</AutoDeleteOnIdle>
    </SubscriptionDescription>
  </content>
</entry>"#;
        let opts = parse_subscription_options(xml).unwrap();
        assert_eq!(opts.lock_duration.as_deref(), Some("PT2M"));
        assert!(opts.requires_session);
        assert_eq!(opts.default_message_ttl.as_deref(), Some("P7D"));
        assert!(opts.dead_lettering_on_message_expiration);
        assert_eq!(opts.dead_lettering_on_filter_evaluation_exceptions, Some(false));
        assert_eq!(opts.max_delivery_count, Some(15));
        assert_eq!(opts.enable_batched_operations, Some(true));
        assert_eq!(opts.status.as_deref(), Some("Active"));
        assert_eq!(opts.forward_to.as_deref(), Some("dest"));
        assert_eq!(opts.user_metadata.as_deref(), Some("team-a"));
        assert_eq!(opts.forward_dead_lettered_messages_to, None);
        assert_eq!(opts.auto_delete_on_idle.as_deref(), Some("PT1H"));
    }
}
