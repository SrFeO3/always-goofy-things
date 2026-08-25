use super::*;
use clap::Parser;

/// Build a Config with the given `--only-tools` allow-list (empty = all tools).
fn cfg(only_tools: Vec<ToolName>) -> Config {
    Config {
        working_dir: ".".to_string(),
        todo_mode: 0,
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
    assert!(msg.content.contains("## 2-3. Information Retrieval (list_directory, grep_search, fetch_web, data_search, data_schema)"));
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
fn system_message_mode1_todo_section_keeps_fixed_number() {
    let config = cfg(vec![ToolName::ReadFile]);
    let msg = system_message_mode1_task_loop(&config);
    assert!(
        msg.content
            .contains("## 4. Todo Context (Plan-Exec Task Loop)"),
        "Todo section must keep its fixed number 4: {}",
        msg.content
    );
}

#[test]
fn system_message_mode2_sections_keep_fixed_numbers() {
    let config = cfg(vec![]);
    let replan = system_message_mode2_replan(&config);
    assert!(
        replan
            .content
            .contains("## 4. Todo Context (Plan-Exec-Dynamic Replan)")
    );
    let task = system_message_mode2_task_loop(&config);
    assert!(
        task.content
            .contains("## 4. Todo Context (Plan-Exec-Dynamic Task Loop)")
    );
}

#[test]
fn todo_system_messages_mention_outputs_lines() {
    let config = cfg(vec![]);
    for msg in [
        system_message_mode1_task_loop(&config),
        system_message_mode2_replan(&config),
        system_message_mode2_task_loop(&config),
    ] {
        assert!(
            msg.content.contains("`outputs:`"),
            "todo system message must mention the outputs: line: {}",
            msg.content
        );
    }
}

#[test]
fn system_message_mode2_uses_next_task_brief() {
    let config = cfg(vec![]);
    let replan = system_message_mode2_replan(&config);
    assert!(
        replan.content.contains("`./next-task.md`"),
        "replan message must direct writing the per-task brief: {}",
        replan.content
    );
    assert!(
        replan
            .content
            .contains("Write ONLY these two files, in this order:"),
        "replan message must list its outputs in order: {}",
        replan.content
    );
    assert!(
        replan.content.contains("is forbidden"),
        "replan message must forbid writing any other file: {}",
        replan.content
    );
    assert!(
        replan
            .content
            .contains("mark each one must-read or optional"),
        "replan message must split the brief's files into must-read / optional: {}",
        replan.content
    );
    let task = system_message_mode2_task_loop(&config);
    assert!(
        task.content.contains("`./next-task.md`"),
        "task message must read the per-task brief: {}",
        task.content
    );
    assert!(
        task.content
            .contains("explore `artifacts/` with list_directory"),
        "task message must allow exploring artifacts/ when the brief is insufficient: {}",
        task.content
    );
    assert!(
        task.content.contains("marked must-read or optional"),
        "task message must mention the must-read / optional markings: {}",
        task.content
    );
    assert!(
        !task.content.contains("Do NOT read `artifacts/handover.md`"),
        "task message must not forbid reading handover.md outright: {}",
        task.content
    );
}
