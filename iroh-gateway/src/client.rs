#[derive(Debug, Clone)]
pub struct Client {}

impl Client {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn get_file(&self, path: &str) -> Result<String, String> {
        Ok(path.to_string())
    }
}
