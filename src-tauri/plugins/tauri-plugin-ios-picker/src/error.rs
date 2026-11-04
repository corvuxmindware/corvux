use serde::{ser::Serializer, Serialize};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("iOS picker plugin is only available on iOS")]
    NotAvailable,

    #[error("user cancelled folder picker")]
    Cancelled,

    #[cfg(mobile)]
    #[error("{0}")]
    PluginInvoke(#[from] tauri::plugin::mobile::PluginInvokeError),

    #[error("{0}")]
    Other(String),
}

impl Serialize for Error {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
