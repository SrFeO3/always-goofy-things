use super::*;
use clap::Parser;

/// Build a Config with the given `--only-tools` allow-list (empty = all tools).
fn cfg(only_tools: Vec<ToolName>) -> Config {
    Config {
        working_dir: ".".to_string(),
        llm_url: "http://localhost:11434/api/chat".to_string(),
        llm_model: "test-model".to_string(),
        llm_api_key: None,
        unsafe_reflex: false,
        verbose_level: 1,
        pretty_level: 1,
        llm_rpm: 0,
        max_output_tokens: 16384,
        max_reasoning_empty_responses: 2,
        session_label: "default".to_string(),
        provider: None,
        provider_extras: Vec::new(),
        tool_result_format: ToolResultFormat::JsonString,
        query: None,
        output_file: None,
        max_reasoning_turns: 30,
        max_replan_attempts: 3,
        max_tool_output_bytes: 1048576,
        tool_timeout_secs: 30,
        db_type: None,
        db_url: None,
        db_auth_key: None,
        db_timeout: 30,
        db_max_bytes: 65536,
        db_unsafe_reflex: false,
        kb_dir: None,
        kb_auto_confirm: KbAutoConfirm::Ask,
        kb_max_bytes: 65536,
        mini_python_auto_confirm: MiniPythonAutoConfirm::Ask,
        only_tools,
        command: None,
    }
}

#[test]
fn is_tool_enabled_all_tools_when_unset() {
    let config = cfg(vec![]);
    for name in [
        "list_directory",
        "read_file",
        "write_file",
        "str_replace_editor",
        "grep_search",
        "execute_bash",
        "fetch_web",
        "data_search",
        "data_schema",
        "calc",
    ] {
        assert!(config.is_tool_enabled(name), "expected '{}' enabled", name);
    }
}

#[test]
fn is_tool_enabled_allow_list() {
    let config = cfg(vec![ToolName::ReadFile, ToolName::ListDirectory]);
    assert!(config.is_tool_enabled("read_file"));
    assert!(config.is_tool_enabled("list_directory"));
    assert!(!config.is_tool_enabled("execute_bash"));
    assert!(!config.is_tool_enabled("write_file"));
    assert!(!config.is_tool_enabled("data_search"));
}

#[test]
fn only_tools_flag_parses_comma_list_and_repeats() {
    let config =
        Config::try_parse_from(["agt", "--only-tools", "read_file,list_directory"]).unwrap();
    assert_eq!(
        config.only_tools,
        vec![ToolName::ReadFile, ToolName::ListDirectory]
    );

    let config = Config::try_parse_from([
        "agt",
        "--only-tools",
        "read_file",
        "--only-tools",
        "execute_bash",
    ])
    .unwrap();
    assert_eq!(
        config.only_tools,
        vec![ToolName::ReadFile, ToolName::ExecuteBash]
    );
}

#[test]
fn only_tools_rejects_unknown_name() {
    let result = Config::try_parse_from(["agt", "--only-tools", "rm_rf"]);
    assert!(result.is_err(), "unknown tool name must fail at parse time");
}

#[test]
fn provider_extras_parses_and_enabled_check() {
    let config = Config::try_parse_from(["agt", "--provider-extras", "opencode"]).unwrap();
    assert!(config.provider_extra_enabled(ProviderExtra::Opencode));

    let config = Config::try_parse_from(["agt"]).unwrap();
    assert!(!config.provider_extra_enabled(ProviderExtra::Opencode));
}

#[test]
fn provider_extras_rejects_unknown_name() {
    let result = Config::try_parse_from(["agt", "--provider-extras", "magic"]);
    assert!(
        result.is_err(),
        "unknown provider extra must fail at parse time"
    );
}

#[test]
fn license_subcommand_parses() {
    let config = Config::try_parse_from(["agt", "license"]).unwrap();
    assert!(matches!(config.command, Some(Command::License)));
}

#[test]
fn no_subcommand_by_default() {
    let config = Config::try_parse_from(["agt"]).unwrap();
    assert!(config.command.is_none());
}

#[test]
fn system_message_full_when_all_enabled() {
    let config = cfg(vec![]);
    let msg = system_message(&config);
    assert!(msg.content.contains("## 1. Workspace Context"));
    assert!(
        msg.content
            .contains("## 2. Tools (your interface to the workspace and the outside world)")
    );
    assert!(
        msg.content
            .contains("## 2-1. Command Execution (execute_bash)")
    );
    assert!(
        msg.content
            .contains("## 2-2. File Operations (read_file, str_replace_editor, write_file)")
    );
    assert!(
        msg.content
            .contains("- str_replace_editor: Replace one exact string block; prefer it over write_file for partial edits.")
    );
    assert!(msg.content.contains("## 2-3. Information Retrieval (list_directory, grep_search, fetch_web, data_search, data_schema"));
    #[cfg(feature = "kb")]
    {
        assert!(
            msg.content.contains("kb_search, kb_schema"),
            "KB read tools must appear in the retrieval section when the kb feature is on"
        );
        assert!(
            msg.content.contains("## 2-5. Knowledge Base (kb_*)"),
            "KB analysis playbook must appear when the kb feature is on"
        );
    }
    assert!(
        msg.content
            .contains("## 2-4. Deterministic Calculation (calc)")
    );
    assert!(
        msg.content
            .contains("verbatim copies of tool execution results"),
        "citation rule must be present when calc is enabled"
    );
    assert!(msg.content.contains("## 3. Response Style"));
}

#[test]
fn system_message_omits_disabled_tools() {
    let config = cfg(vec![
        ToolName::ReadFile,
        ToolName::ListDirectory,
        ToolName::GrepSearch,
    ]);
    let msg = system_message(&config);
    assert!(msg.content.contains("## 1. Workspace Context"));
    assert!(msg.content.contains("## 2. Tools"));
    assert!(msg.content.contains("## 2-2. File Operations (read_file)"));
    assert!(
        msg.content
            .contains("## 2-3. Information Retrieval (list_directory, grep_search)")
    );
    assert!(msg.content.contains("## 3. Response Style"));
    assert!(!msg.content.contains("## 2-1."));
    assert!(!msg.content.contains("Command Execution"));
    assert!(!msg.content.contains("write_file"));
    assert!(!msg.content.contains("str_replace_editor"));
    assert!(!msg.content.contains("fetch_web"));
    assert!(!msg.content.contains("data_search"));
    assert!(!msg.content.contains("data_schema"));
    assert!(!msg.content.contains("## 2-5."));
    assert!(
        !msg.content.contains("calc"),
        "calc must be omitted when disabled"
    );
    assert!(
        !msg.content.contains("verbatim copies"),
        "citation rule must be omitted when calc is disabled"
    );
}

#[test]
fn system_message_lists_only_enabled_editors() {
    let config = cfg(vec![ToolName::WriteFile]);
    let msg = system_message(&config);
    assert!(
        msg.content.contains("## 2-2. File Operations (write_file)"),
        "write_file keeps its fixed section: {}",
        msg.content
    );
    assert!(
        msg.content.contains(
            "- write_file: Create a new file or fully replace an existing one; for new files and full rewrites only."
        ),
        "write_file rule must be listed: {}",
        msg.content
    );
    assert!(!msg.content.contains("## 2-1."));
    assert!(!msg.content.contains("## 2-3."));
    assert!(!msg.content.contains("read_file"));
    assert!(!msg.content.contains("str_replace_editor"));
}

#[test]
fn system_message_todo_task_keeps_fixed_number_and_narrows() {
    let config = cfg(vec![ToolName::ReadFile]);
    let msg = system_message_todo_task(&config, None);
    assert!(
        msg.content.contains("## 4. Todo Context (Task)"),
        "Todo section must keep its fixed number 4: {}",
        msg.content
    );
    assert!(msg.content.contains("Handover Report"));
    let narrowed = system_message_todo_task(&cfg(vec![]), Some(&["read_file"]));
    assert!(narrowed.content.contains("read_file"));
    assert!(!narrowed.content.contains("write_file"));
}

#[test]
fn system_message_todo_planner_is_read_only() {
    let config = cfg(vec![]);
    let msg = system_message_todo_planner(&config);
    assert!(msg.content.contains("## 4. Todo Context (Replan Planner)"));
    assert!(msg.content.contains("```todo-plan"));
    assert!(!msg.content.contains("write_file"));
    assert!(!msg.content.contains("execute_bash"));
}

#[test]
fn todo_system_messages_mention_handover_report() {
    let config = cfg(vec![]);
    let msg = system_message_todo_task(&config, None);
    assert!(
        msg.content.contains("Handover Report"),
        "todo task message must name the report contract: {}",
        msg.content
    );
}

#[cfg(feature = "kb")]
#[test]
fn system_message_kb_extract_forbids_self_declared_analyzed() {
    let config = cfg(vec![]);
    let msg = system_message_kb_extract(&config);
    assert!(msg.content.contains("## 4. KB Context (Extract"));
    assert!(msg.content.contains("NEVER set `analysis_status`"));
    assert!(msg.content.contains("Extraction Report"));
    assert!(!msg.content.contains("write_file"));
}
