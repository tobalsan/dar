#[tokio::main(worker_threads = 2)]
async fn main() {
    agentropy_cli::run(host_api::plugins![
        tool_registry_host::ToolRegistryHostExtension,
        frontend_log::FrontendLogExtension,
        tracker_files::TrackerFilesExtension,
        tracker_linear::TrackerLinearExtension,
        orchestrator::OrchestratorExtension::default(),
        dashboard::DashboardExtension::default(),
        runner_pi::RunnerPiExtension,
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

#[cfg(test)]
mod tests {
    #[test]
    fn dist_delegates_to_agentropy_cli_run() {
        let source = include_str!("main.rs");
        let non_test_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("main source before tests");
        assert!(non_test_source.contains("agentropy_cli::run("));
    }

    #[test]
    fn stock_plugin_list_order_stays_stable() {
        let source = include_str!("main.rs");
        let plugins = source
            .lines()
            .skip_while(|line| !line.contains("host_api::plugins!["))
            .skip(1)
            .take_while(|line| !line.contains("]"))
            .map(|line| line.trim().trim_end_matches(','))
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            plugins,
            [
                "tool_registry_host::ToolRegistryHostExtension",
                "frontend_log::FrontendLogExtension",
                "tracker_files::TrackerFilesExtension",
                "tracker_linear::TrackerLinearExtension",
                "orchestrator::OrchestratorExtension::default()",
                "dashboard::DashboardExtension::default()",
                "runner_pi::RunnerPiExtension",
                "runner_codex::RunnerCodexExtension",
                "runner_opencode::RunnerOpenCodeExtension",
                "runner_cli::RunnerCliExtension",
                "runner_fake::RunnerFakeExtension",
                "chat_opencode::ChatOpenCodeExtension",
                "chat_pi::ChatPiExtension",
                "chat_codex::ChatCodexExtension",
                "tui::TuiExtension",
            ]
        );
    }
}
