use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::{
    contracts::json as json_contract,
    domain::{BridgeError, ToolSet},
};

const CODEX_TOOL_ALIAS_PREFIX: &str = "model_rocket_tool_";
const CLAUDE_TOOL_DESCRIPTION_PREFIX: &str = "Claude Code tool name: ";
// Bounds the Claude-side name, which never reaches Codex: every tool is aliased
// to `model_rocket_tool_<index>` and the original travels only inside the alias
// description. So this is a sanity bound on text the bridge echoes back, not the
// 64-byte function-name limit the Responses API applies to the alias itself.
// MCP servers routinely exceed 64 — `mcp__<server>__<action>__<VERB_NOUN_NOUN>`
// reaches into the seventies — and refusing one refused the whole request,
// because Claude Code sends its entire tool set every turn. A single long-named
// MCP tool therefore made every turn fail, including turns that used no tool.
// 128 matches Anthropic's own tool-name limit, which the name has already
// satisfied by the time Claude Code sends it.
const MAX_CLAUDE_TOOL_NAME_BYTES: usize = 128;

#[derive(Debug)]
pub(crate) struct CodexDynamicTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Default)]
pub(crate) struct DynamicToolNames {
    tools: Vec<CodexDynamicTool>,
    claude_name_by_codex_name: HashMap<String, String>,
}

impl DynamicToolNames {
    pub(crate) fn from_claude_tools(tools: &ToolSet) -> Result<Self, BridgeError> {
        let tools = tools.as_slice();
        let mut seen_claude_names = HashSet::with_capacity(tools.len());
        let mut codex_tools = Vec::with_capacity(tools.len());
        let mut claude_name_by_codex_name = HashMap::with_capacity(tools.len());

        for (index, tool) in tools.iter().enumerate() {
            let claude_name = tool.name().as_str();
            if !valid_claude_tool_name(claude_name) {
                return Err(BridgeError::invalid_request(format!(
                    "invalid Claude tool name {claude_name}"
                )));
            }
            if !seen_claude_names.insert(claude_name) {
                return Err(BridgeError::invalid_request(format!(
                    "duplicate Claude tool name {claude_name}"
                )));
            }

            let codex_name = format!("{CODEX_TOOL_ALIAS_PREFIX}{index}");
            let description = format!(
                "{CLAUDE_TOOL_DESCRIPTION_PREFIX}{}\n\n{}",
                claude_name,
                tool.description().as_str()
            );
            claude_name_by_codex_name.insert(codex_name.clone(), claude_name.to_owned());
            codex_tools.push(CodexDynamicTool {
                name: codex_name,
                description,
                input_schema: json_contract::object_value(tool.input_schema()).map_err(
                    |error| BridgeError::protocol(format!("tool input schema is invalid: {error}")),
                )?,
            });
        }

        Ok(Self {
            tools: codex_tools,
            claude_name_by_codex_name,
        })
    }

    pub(crate) fn tools(&self) -> &[CodexDynamicTool] {
        &self.tools
    }

    pub(crate) fn claude_name(&self, codex_name: &str) -> Result<&str, BridgeError> {
        self.claude_name_by_codex_name
            .get(codex_name)
            .map(String::as_str)
            .ok_or_else(|| {
                BridgeError::protocol(format!("Codex requested unknown dynamic tool {codex_name}"))
            })
    }
}

fn valid_claude_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_CLAUDE_TOOL_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{DynamicToolNames, MAX_CLAUDE_TOOL_NAME_BYTES};
    use crate::{
        contracts::json as json_contract,
        domain::{ClaudeToolName, ToolDefinition, ToolDescription, ToolSet},
    };

    fn tool(name: &str, description: &str) -> Result<ToolDefinition, Box<dyn std::error::Error>> {
        Ok(ToolDefinition::new(
            ClaudeToolName::from(name),
            ToolDescription::new(description),
            json_contract::object(&json!({"type": "object"}))?,
        ))
    }

    #[test]
    fn long_mcp_tool_name_is_aliased_rather_than_rejected() -> Result<(), Box<dyn std::error::Error>>
    {
        // An MCP-namespaced action name past the previous bound. It reaches
        // Codex only as an alias, so its length is no reason to refuse it.
        let original = "mcp__compliance_platform__action__BULK_UPSERT_QUESTIONNAIRE_ANSWERS";
        assert!(
            original.len() > 64,
            "fixture must exceed the previous bound"
        );
        let names = DynamicToolNames::from_claude_tools(&ToolSet::new(vec![tool(
            original,
            "Upsert questions and answers",
        )?]))?;
        let mapped = names.tools().first().ok_or("the tool was not mapped")?;
        assert_eq!(mapped.name, "model_rocket_tool_0");
        assert_eq!(names.claude_name("model_rocket_tool_0")?, original);

        // Pin the accept side of the boundary too. Without it a regression that
        // shrank the effective bound would still pass every other test, and
        // re-break exactly the names this bound was raised for.
        let at_bound = "x".repeat(MAX_CLAUDE_TOOL_NAME_BYTES);
        let names = DynamicToolNames::from_claude_tools(&ToolSet::new(vec![tool(&at_bound, "")?]))?;
        assert_eq!(names.claude_name("model_rocket_tool_0")?, at_bound);
        Ok(())
    }

    #[test]
    fn reserved_claude_name_is_aliased_and_restored() -> Result<(), Box<dyn std::error::Error>> {
        let original = "mcp__plugin_context7_context7__query-docs";
        let names = DynamicToolNames::from_claude_tools(&ToolSet::new(vec![tool(
            original,
            "Query documentation",
        )?]))?;

        let mapped = names.tools().first().ok_or("mapped tool was not created")?;
        assert_eq!(mapped.name, "model_rocket_tool_0");
        assert!(!mapped.name.starts_with("mcp__"));
        assert_eq!(names.claude_name(&mapped.name)?, original);
        assert!(mapped.description.contains(original));
        Ok(())
    }

    #[test]
    fn duplicate_claude_names_fail_explicitly() -> Result<(), Box<dyn std::error::Error>> {
        let tool = tool("Read", "")?;

        let Err(error) =
            DynamicToolNames::from_claude_tools(&ToolSet::new(vec![tool.clone(), tool]))
        else {
            return Err("duplicate tool names were accepted".into());
        };
        assert_eq!(
            error.to_string(),
            "invalid request: duplicate Claude tool name Read"
        );
        Ok(())
    }

    #[test]
    fn invalid_claude_names_fail_before_aliasing() -> Result<(), Box<dyn std::error::Error>> {
        // The over-length case is expressed against the bound itself, so it
        // keeps testing "one byte too long" if the bound ever moves again.
        let too_long = "x".repeat(MAX_CLAUDE_TOOL_NAME_BYTES + 1);
        for invalid_name in ["", "contains space", "contains.dot", &too_long] {
            let Err(error) =
                DynamicToolNames::from_claude_tools(&ToolSet::new(vec![tool(invalid_name, "")?]))
            else {
                return Err(
                    format!("invalid Claude tool name was accepted: {invalid_name}").into(),
                );
            };
            assert!(error.to_string().contains("invalid Claude tool name"));
        }
        Ok(())
    }

    #[test]
    fn unknown_codex_name_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let names = DynamicToolNames::default();
        let Err(error) = names.claude_name("Read") else {
            return Err("unknown Codex tool name was accepted".into());
        };
        assert_eq!(
            error.to_string(),
            "App Server protocol error: Codex requested unknown dynamic tool Read"
        );
        Ok(())
    }
}
