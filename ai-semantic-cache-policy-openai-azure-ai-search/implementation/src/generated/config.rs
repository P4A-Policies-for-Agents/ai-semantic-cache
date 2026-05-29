use serde::Deserialize;
#[derive(Deserialize, Clone, Debug)]
pub struct EmbedderConfig {
    #[serde(alias = "apiKey")]
    pub api_key: Option<String>,
    #[serde(alias = "authHeader")]
    pub auth_header: Option<String>,
    #[serde(alias = "authScheme")]
    pub auth_scheme: Option<String>,
    #[serde(alias = "baseUrl", deserialize_with = "pdk::serde::deserialize_service")]
    pub base_url: pdk::hl::Service,
    #[serde(alias = "dimension")]
    pub dimension: i64,
    #[serde(alias = "model")]
    pub model: String,
    #[serde(alias = "timeoutMs")]
    pub timeout_ms: Option<i64>,
}
#[derive(Deserialize, Clone, Debug)]
pub struct VectordbConfig {
    #[serde(alias = "apiKey")]
    pub api_key: String,
    #[serde(alias = "apiVersion")]
    pub api_version: Option<String>,
    #[serde(alias = "endpoint", deserialize_with = "pdk::serde::deserialize_service")]
    pub endpoint: pdk::hl::Service,
    #[serde(alias = "indexName")]
    pub index_name: String,
    #[serde(alias = "timeoutMs")]
    pub timeout_ms: Option<i64>,
    #[serde(alias = "vectorField")]
    pub vector_field: Option<String>,
}
#[derive(Deserialize, Clone, Debug)]
pub struct Config {
    #[serde(alias = "bypassOnStream")]
    pub bypass_on_stream: Option<bool>,
    #[serde(alias = "cacheResponseStatusCodes")]
    pub cache_response_status_codes: Option<Vec<i64>>,
    #[serde(alias = "embedder")]
    pub embedder: EmbedderConfig,
    #[serde(alias = "exactCaching")]
    pub exact_caching: Option<bool>,
    #[serde(alias = "failureMode")]
    pub failure_mode: Option<String>,
    #[serde(alias = "namespace")]
    pub namespace: String,
    #[serde(alias = "promptExtractor")]
    pub prompt_extractor: Option<String>,
    #[serde(alias = "similarityThreshold")]
    pub similarity_threshold: Option<f64>,
    #[serde(alias = "ttlSeconds")]
    pub ttl_seconds: Option<i64>,
    #[serde(alias = "vectordb")]
    pub vectordb: VectordbConfig,
}
#[pdk::hl::entrypoint_flex]
fn init(abi: &dyn pdk::flex_abi::api::FlexAbi) -> Result<(), anyhow::Error> {
    let config: Config = serde_json::from_slice(abi.get_configuration())
        .map_err(|err| {
            anyhow::anyhow!(
                "Failed to parse configuration '{}'. Cause: {}",
                String::from_utf8_lossy(abi.get_configuration()), err
            )
        })?;
    let current = config.embedder;
    abi.service_create(current.base_url)?;
    let current = config.vectordb;
    abi.service_create(current.endpoint)?;
    abi.setup()?;
    Ok(())
}
