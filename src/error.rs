use derive_more::Display;

#[derive(Debug, Display)]
pub enum AppError {
    #[display("Failed to initialize application")]
    Initialization,

    #[display("GitHub API error")]
    GitHubApi,

    #[display("Failed to search for PRs")]
    Search,

    #[display("Failed to comment on PR")]
    Comment,

    #[display("Interactive prompt failed")]
    Interactive,

    #[display("Invalid input provided")]
    InvalidInput,

    #[display("Action selection failed")]
    ActionSelection,
}

impl_more::impl_leaf_error!(AppError);
