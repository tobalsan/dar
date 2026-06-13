// # generated - do not hand-edit
#[tokio::main(worker_threads = 2)]
async fn main() {
    agentropy_cli::run(host_api::plugins![
        frontend_log::FrontendLogExtension,
        tracker_files::TrackerFilesExtension,
        tracker_linear::TrackerLinearExtension,
        orchestrator::OrchestratorExtension::default(),
        dashboard::DashboardExtension::default(),
        runner_pi::RunnerPiExtension,
        runner_claude::RunnerClaudeExtension,
        runner_codex::RunnerCodexExtension,
        runner_opencode::RunnerOpenCodeExtension,
        runner_cli::RunnerCliExtension,
        runner_fake::RunnerFakeExtension,
        chat_opencode::ChatOpenCodeExtension,
        chat_pi::ChatPiExtension,
        chat_codex::ChatCodexExtension,
        tui::TuiExtension,
    ])
    .await
}
