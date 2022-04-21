use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Config {
    pub writeable: bool,
    pub fetch: bool,
    pub cache: bool,
    pub headers: HashMap<String, String>,
    pub port: String,
}
