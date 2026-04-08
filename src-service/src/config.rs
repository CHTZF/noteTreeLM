use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub service: ServiceConfig,
    #[serde(default)]
    pub servers: ServersConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceConfig {
    pub port: u16,
    pub https_port: u16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServersConfig {
    pub llama_port: u16,
    pub embedding_port: u16,
    pub whisper_port: u16,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self { port: 7787, https_port: 7788 }
    }
}

impl Default for ServersConfig {
    fn default() -> Self {
        Self { llama_port: 18080, embedding_port: 18081, whisper_port: 18082 }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self { service: ServiceConfig::default(), servers: ServersConfig::default() }
    }
}

impl Config {
    /// 從 config dir 讀取 config.toml。
    /// 檔案不存在時 error（應由安裝包建立）；格式錯誤時 error。
    pub fn load(config_dir: &Path) -> Result<Self, String> {
        let path = config_dir.join("config.toml");
        let contents = std::fs::read_to_string(&path)
            .map_err(|e| format!("無法讀取設定檔 {}: {}", path.display(), e))?;
        toml::from_str(&contents)
            .map_err(|e| format!("設定檔格式錯誤: {}", e))
    }

    /// 若 config.toml 不存在則建立預設值（供開發環境使用）。
    pub fn load_or_default(config_dir: &Path) -> Self {
        let path = config_dir.join("config.toml");
        if path.exists() {
            match Self::load(config_dir) {
                Ok(c) => return c,
                Err(e) => tracing::warn!("設定檔讀取失敗，使用預設值：{}", e),
            }
        }
        tracing::info!("設定檔不存在，使用預設值（{}))", path.display());
        Self::default()
    }

    pub fn llm_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.servers.llama_port)
    }

    pub fn embedding_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.servers.embedding_port)
    }

    pub fn whisper_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.servers.whisper_port)
    }
}
