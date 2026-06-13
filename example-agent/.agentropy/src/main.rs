// # generated - do not hand-edit
#[tokio::main(worker_threads = 2)]
async fn main() {
    agentropy_cli::run(host_api::plugins![
        #[cfg(feature = "stock-chat-pi")]
        chat_pi::ChatPiExtension,
        #[cfg(feature = "stock-frontend-log")]
        frontend_log::FrontendLogExtension,
        #[cfg(feature = "stock-orchestrator")]
        orchestrator::OrchestratorExtension::default(),
        #[cfg(feature = "stock-runner-claude")]
        runner_claude::RunnerClaudeExtension,
        #[cfg(feature = "stock-tracker-files")]
        tracker_files::TrackerFilesExtension,
        #[cfg(feature = "stock-tui")]
        tui::TuiExtension,
    ])
    .await
}
