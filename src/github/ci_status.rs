use console::style;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CiStatus {
    Passing,
    Failing,
    Pending,
    Unknown,
}

impl std::fmt::Display for CiStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CiStatus::Passing => write!(f, "passing"),
            CiStatus::Failing => write!(f, "failing"),
            CiStatus::Pending => write!(f, "pending"),
            CiStatus::Unknown => write!(f, "unknown"),
        }
    }
}

impl CiStatus {
    pub fn icon(self) -> console::StyledObject<&'static str> {
        match self {
            CiStatus::Passing => style("✓").green(),
            CiStatus::Failing => style("✗").red(),
            CiStatus::Pending => style("●").yellow(),
            CiStatus::Unknown => style("○").dim(),
        }
    }

    pub fn is_mergeable(self) -> bool {
        matches!(self, CiStatus::Passing | CiStatus::Unknown)
    }
}
