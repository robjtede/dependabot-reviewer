use console::style;

use crate::github::CiStatus;

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub number: u64,
    pub title: String,
    pub url: String,
    #[allow(dead_code)]
    pub head_sha: String,
    pub ci_status: CiStatus,
}

impl PrInfo {
    pub fn display(&self) -> String {
        format!(
            "{} #{}: {}\n        {}",
            self.ci_status.icon(),
            self.number,
            self.title,
            style(&self.url).dim()
        )
    }
}
