use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, Url};
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone)]
pub struct MihomoClient {
    client: Client,
    base: Url,
    secret: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct VersionInfo {
    #[serde(default)]
    pub version: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub mode: String,
    #[serde(rename = "mixed-port")]
    pub mixed_port: Option<u16>,
    #[serde(rename = "allow-lan")]
    pub allow_lan: Option<bool>,
    pub ipv6: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ProxyResponse {
    #[serde(default)]
    pub proxies: HashMap<String, Proxy>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Proxy {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub now: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub all: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub history: Vec<DelayHistory>,
    #[serde(default)]
    pub alive: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DelayHistory {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub delay: u32,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ConnectionResponse {
    #[serde(rename = "downloadTotal", alias = "download_total", default)]
    pub download_total: u64,
    #[serde(rename = "uploadTotal", alias = "upload_total", default)]
    pub upload_total: u64,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub connections: Vec<Connection>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Connection {
    #[serde(default)]
    pub id: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub metadata: Metadata,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub chains: Vec<String>,
    #[serde(default)]
    pub upload: u64,
    #[serde(default)]
    pub download: u64,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Metadata {
    #[serde(
        rename = "host",
        alias = "hostname",
        default,
        deserialize_with = "deserialize_null_default"
    )]
    pub host: String,
    #[serde(
        rename = "destinationIP",
        default,
        deserialize_with = "deserialize_null_default"
    )]
    pub destination_ip: String,
    #[serde(
        rename = "destinationPort",
        default,
        deserialize_with = "deserialize_null_default"
    )]
    pub destination_port: String,
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub network: String,
    #[serde(
        rename = "type",
        default,
        deserialize_with = "deserialize_null_default"
    )]
    pub kind: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RuleResponse {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    pub rules: Vec<Rule>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Rule {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub payload: String,
    #[serde(default)]
    pub proxy: String,
}

#[derive(Debug, Deserialize)]
struct DelayResponse {
    delay: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub version: VersionInfo,
    pub config: RuntimeConfig,
    pub proxies: ProxyResponse,
    pub connections: ConnectionResponse,
    pub rules: RuleResponse,
}

impl MihomoClient {
    pub fn new(controller: &str, secret: String) -> Result<Self> {
        let base = Url::parse(&format!("{}/", controller.trim_end_matches('/')))
            .context("controller must be a valid HTTP URL")?;
        Ok(Self {
            client: Client::builder()
                // The controller belongs to the local Mihomo process managed by omash. In
                // particular, never inherit HTTP_PROXY/ALL_PROXY here: doing
                // so sends 127.0.0.1 requests to an upstream proxy and turns
                // an otherwise healthy Mihomo into an apparent 502.
                .no_proxy()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(5))
                .build()?,
            base,
            secret,
        })
    }

    fn url(&self, segments: &[&str]) -> Result<Url> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("controller URL cannot be a base"))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<T> {
        let mut request = self.client.request(method, self.url(segments)?);
        if !self.secret.is_empty() {
            request = request.bearer_auth(&self.secret);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.context("cannot connect to Mihomo")?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            bail!(
                "Mihomo returned {status}: {}",
                String::from_utf8_lossy(&bytes)
            );
        }
        if bytes.is_empty() {
            return serde_json::from_value(Value::Null).context("empty response");
        }
        serde_json::from_slice(&bytes).map_err(|error| {
            anyhow::anyhow!(
                "invalid response from Mihomo at /{}: {error}",
                segments.join("/")
            )
        })
    }

    async fn empty(&self, method: Method, segments: &[&str], body: Option<Value>) -> Result<()> {
        let mut request = self.client.request(method, self.url(segments)?);
        if !self.secret.is_empty() {
            request = request.bearer_auth(&self.secret);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.context("cannot connect to Mihomo")?;
        let status = response.status();
        if !status.is_success() {
            bail!(
                "Mihomo returned {status}: {}",
                response.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }

    pub async fn snapshot(&self) -> Result<Snapshot> {
        let (version, config, proxies, connections, rules) = tokio::try_join!(
            self.request(Method::GET, &["version"], None),
            self.request(Method::GET, &["configs"], None),
            self.request(Method::GET, &["proxies"], None),
            self.request(Method::GET, &["connections"], None),
            self.request(Method::GET, &["rules"], None),
        )?;
        Ok(Snapshot {
            version,
            config,
            proxies,
            connections,
            rules,
        })
    }

    pub async fn version(&self) -> Result<VersionInfo> {
        self.request(Method::GET, &["version"], None).await
    }

    pub async fn runtime_config(&self) -> Result<RuntimeConfig> {
        self.request(Method::GET, &["configs"], None).await
    }

    pub async fn proxies(&self) -> Result<ProxyResponse> {
        self.request(Method::GET, &["proxies"], None).await
    }

    pub async fn select_proxy(&self, group: &str, proxy: &str) -> Result<()> {
        self.empty(
            Method::PUT,
            &["proxies", group],
            Some(json!({ "name": proxy })),
        )
        .await
    }

    pub async fn test_delay(&self, proxy: &str, target: &str) -> Result<u32> {
        let mut url = self.url(&["proxies", proxy, "delay"])?;
        url.query_pairs_mut()
            .append_pair("timeout", "5000")
            .append_pair("url", target);
        let mut request = self.client.get(url).timeout(Duration::from_secs(8));
        if !self.secret.is_empty() {
            request = request.bearer_auth(&self.secret);
        }
        let response: DelayResponse = request.send().await?.error_for_status()?.json().await?;
        Ok(response.delay)
    }

    pub async fn test_group_delay(
        &self,
        group: &str,
        target: &str,
    ) -> Result<HashMap<String, u32>> {
        let mut url = self.url(&["group", group, "delay"])?;
        url.query_pairs_mut()
            .append_pair("timeout", "5000")
            .append_pair("url", target);
        let mut request = self.client.get(url).timeout(Duration::from_secs(8));
        if !self.secret.is_empty() {
            request = request.bearer_auth(&self.secret);
        }
        Ok(request.send().await?.error_for_status()?.json().await?)
    }

    pub async fn set_mode(&self, mode: &str) -> Result<()> {
        self.empty(Method::PATCH, &["configs"], Some(json!({ "mode": mode })))
            .await
    }

    pub async fn reload_config(&self, path: &std::path::Path) -> Result<()> {
        let mut url = self.url(&["configs"])?;
        url.query_pairs_mut().append_pair("force", "true");
        let mut request = self.client.put(url);
        if !self.secret.is_empty() {
            request = request.bearer_auth(&self.secret);
        }
        let response = request
            .json(&json!({ "path": path }))
            .send()
            .await
            .context("cannot ask Mihomo to reload configuration")?;
        let status = response.status();
        if !status.is_success() {
            bail!(
                "Mihomo returned {status} while reloading configuration: {}",
                response.text().await.unwrap_or_default()
            );
        }
        Ok(())
    }

    pub async fn close_connection(&self, id: Option<&str>) -> Result<()> {
        let path = id.map_or_else(|| vec!["connections"], |id| vec!["connections", id]);
        self.empty(Method::DELETE, &path, None).await
    }
}

fn deserialize_null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encodes_proxy_names() {
        let client = MihomoClient::new("http://127.0.0.1:9090", String::new()).unwrap();
        let url = client.url(&["proxies", "香港 / 01"]).unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:9090/proxies/%E9%A6%99%E6%B8%AF%20%2F%2001"
        );
    }

    #[test]
    fn flexible_connection_metadata() {
        let value = r#"{"id":"1","metadata":{"host":"example.com","destinationPort":"443"}}"#;
        let item: Connection = serde_json::from_str(value).unwrap();
        assert_eq!(item.metadata.host, "example.com");
        assert_eq!(item.metadata.destination_port, "443");
    }

    #[test]
    fn accepts_nullable_mihomo_collections() {
        let proxies: ProxyResponse = serde_json::from_str(
            r#"{"proxies":{"node":{"type":"Compatible","now":null,"all":null}}}"#,
        )
        .unwrap();
        let node = &proxies.proxies["node"];
        assert!(node.now.is_empty());
        assert!(node.all.is_empty());

        let connections: ConnectionResponse =
            serde_json::from_str(r#"{"connections":null}"#).unwrap();
        assert!(connections.connections.is_empty());
    }
}
