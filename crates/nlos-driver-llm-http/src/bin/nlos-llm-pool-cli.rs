//! Operational CLI (`nlos-llm-pool-cli`) for probing local `glm-keypool` status,
//! executing 5D-budget-metered chat completions, and compiling structured NLOS
//! intents (`W77-PRE-B` / `E0`).

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_driver_llm_http::{
    LlmChatRequest, LlmDriverError, LlmEnforcementShim, MeteredBudgetContext, current_wall_ms,
    derive_idempotency_key, derive_operation_id, discover_local_keypool,
    ensure_default_trial_llm_budget, format_receipt_id_hex,
};
use nlos_resource::ResourceAuthority;

const REASONING_EXCERPT_MAX_CHARS: usize = 240;

#[derive(Debug)]
enum CliCommand {
    Help,
    Status {
        pool_config: Option<PathBuf>,
    },
    Chat {
        prompt: String,
        model: Option<String>,
        max_tokens: Option<u32>,
        session: Option<String>,
        root: Option<PathBuf>,
        pool_config: Option<PathBuf>,
    },
    Intent {
        query: String,
        locale: String,
        model: Option<String>,
        root: Option<PathBuf>,
        pool_config: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run_cli(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

async fn run_cli(args: &[String]) -> Result<(), String> {
    let cmd = parse_cli_args(args)?;
    match cmd {
        CliCommand::Help => {
            print_usage();
            Ok(())
        }
        CliCommand::Status { pool_config } => run_status(pool_config.as_deref()).await,
        CliCommand::Chat {
            prompt,
            model,
            max_tokens,
            session,
            root,
            pool_config,
        } => {
            run_chat(
                &prompt,
                model.as_deref(),
                max_tokens,
                session.as_deref(),
                root.as_deref(),
                pool_config.as_deref(),
            )
            .await
        }
        CliCommand::Intent {
            query,
            locale,
            model,
            root,
            pool_config,
        } => {
            run_intent(
                &query,
                &locale,
                model.as_deref(),
                root.as_deref(),
                pool_config.as_deref(),
            )
            .await
        }
    }
}

fn parse_cli_args(args: &[String]) -> Result<CliCommand, String> {
    let Some(sub) = args.first().map(String::as_str) else {
        return Ok(CliCommand::Help);
    };
    match sub {
        "-h" | "--help" | "help" => Ok(CliCommand::Help),
        "status" => parse_status_args(&args[1..]),
        "chat" => parse_chat_args(&args[1..]),
        "intent" => parse_intent_args(&args[1..]),
        other => Err(format!(
            "unknown subcommand '{other}'; run with --help for usage"
        )),
    }
}

fn parse_status_args(rest: &[String]) -> Result<CliCommand, String> {
    let mut pool_config = None;
    let mut idx = 0;
    while idx < rest.len() {
        match rest[idx].as_str() {
            "--pool-config" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --pool-config".to_owned())?;
                pool_config = Some(PathBuf::from(val));
            }
            "-h" | "--help" => return Ok(CliCommand::Help),
            flag => return Err(format!("unknown flag '{flag}' for 'status'")),
        }
        idx += 1;
    }
    Ok(CliCommand::Status { pool_config })
}

fn parse_chat_args(rest: &[String]) -> Result<CliCommand, String> {
    let mut prompt = None;
    let mut model = None;
    let mut max_tokens = None;
    let mut session = None;
    let mut root = None;
    let mut pool_config = None;
    let mut idx = 0;
    while idx < rest.len() {
        match rest[idx].as_str() {
            "--prompt" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --prompt".to_owned())?;
                prompt = Some(val.clone());
            }
            "--model" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --model".to_owned())?;
                model = Some(val.clone());
            }
            "--max-tokens" => {
                idx += 1;
                let raw = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --max-tokens".to_owned())?;
                let parsed = raw
                    .parse::<u32>()
                    .map_err(|_| format!("--max-tokens expects a positive u32, got '{raw}'"))?;
                if parsed == 0 {
                    return Err("--max-tokens must be greater than zero".to_owned());
                }
                max_tokens = Some(parsed);
            }
            "--session" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --session".to_owned())?;
                session = Some(val.clone());
            }
            "--root" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --root".to_owned())?;
                root = Some(PathBuf::from(val));
            }
            "--pool-config" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --pool-config".to_owned())?;
                pool_config = Some(PathBuf::from(val));
            }
            "-h" | "--help" => return Ok(CliCommand::Help),
            flag => return Err(format!("unknown flag '{flag}' for 'chat'")),
        }
        idx += 1;
    }
    let prompt = prompt
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "chat requires non-empty --prompt <TEXT>".to_owned())?;
    Ok(CliCommand::Chat {
        prompt,
        model,
        max_tokens,
        session,
        root,
        pool_config,
    })
}

fn parse_intent_args(rest: &[String]) -> Result<CliCommand, String> {
    let mut query = None;
    let mut locale = "zh-CN".to_owned();
    let mut model = None;
    let mut root = None;
    let mut pool_config = None;
    let mut idx = 0;
    while idx < rest.len() {
        match rest[idx].as_str() {
            "--query" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --query".to_owned())?;
                query = Some(val.clone());
            }
            "--locale" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --locale".to_owned())?;
                locale.clone_from(val);
            }
            "--model" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --model".to_owned())?;
                model = Some(val.clone());
            }
            "--root" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --root".to_owned())?;
                root = Some(PathBuf::from(val));
            }
            "--pool-config" => {
                idx += 1;
                let val = rest
                    .get(idx)
                    .ok_or_else(|| "missing value after --pool-config".to_owned())?;
                pool_config = Some(PathBuf::from(val));
            }
            "-h" | "--help" => return Ok(CliCommand::Help),
            flag => return Err(format!("unknown flag '{flag}' for 'intent'")),
        }
        idx += 1;
    }
    let query = query
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| "intent requires non-empty --query <TEXT>".to_owned())?;
    Ok(CliCommand::Intent {
        query,
        locale,
        model,
        root,
        pool_config,
    })
}

async fn run_status(pool_config: Option<&Path>) -> Result<(), String> {
    let discovered = discover_local_keypool(pool_config).map_err(|e| e.to_string())?;
    println!("discovery_source: {}", discovered.source.redacted_label());
    println!("openai_base_url: {}", discovered.openai_base_url);
    println!("anthropic_base_url: {}", discovered.anthropic_base_url);
    println!(
        "pool_status_url: {}",
        discovered.pool_status_url.as_deref().unwrap_or("<none>")
    );
    println!("default_model: {}", discovered.default_model);
    println!("fast_intent_model: {}", discovered.fast_intent_model);
    println!("allowed_models: [{}]", discovered.allowed_models.join(", "));
    println!("configured_key_slots: {}", discovered.configured_key_slots);
    println!(
        "egress_auth: {}",
        if discovered.egress_key.is_empty() {
            "none"
        } else {
            "configured (<redacted-credential>)"
        }
    );

    let shim = LlmEnforcementShim::from_discovered(discovered);
    match shim.probe_pool_status().await {
        Ok(snap) => {
            println!("online_pool_status: REACHABLE");
            println!("pool_strategy: {}", snap.strategy);
            println!("pool_keys: {}/{} active", snap.active_keys, snap.total_keys);
            println!("pool_active_sessions: {}", snap.active_sessions);
            println!(
                "pool_requests: total={} ok={} failed={}",
                snap.total_requests, snap.ok_requests, snap.failed_requests
            );
            println!("pool_tokens: in={} out={}", snap.tokens_in, snap.tokens_out);
        }
        Err(err) => {
            println!("online_pool_status: UNREACHABLE ({err})");
        }
    }
    Ok(())
}

async fn run_chat(
    prompt: &str,
    model: Option<&str>,
    max_tokens: Option<u32>,
    session: Option<&str>,
    root: Option<&Path>,
    pool_config: Option<&Path>,
) -> Result<(), String> {
    let shim = LlmEnforcementShim::discover(pool_config).map_err(|e| e.to_string())?;
    let mut req = LlmChatRequest::single_user_prompt(prompt);
    if let Some(m) = model.filter(|s| !s.trim().is_empty()) {
        req = req.with_model(m);
    }
    if let Some(limit) = max_tokens {
        req = req.with_max_tokens(limit);
    }
    if let Some(sid) = session.filter(|s| !s.trim().is_empty()) {
        req = req.with_session_id(sid);
    }

    let resource_holder = open_optional_resource_root(root).map_err(|e| e.to_string())?;
    let outcome = if let Some(opened) = &resource_holder {
        shim.execute_metered_chat(Some(opened.as_context()), &req)
            .await
            .map_err(|e| e.to_string())?
    } else {
        shim.execute_metered_chat(None, &req)
            .await
            .map_err(|e| e.to_string())?
    };

    println!("model: {}", outcome.model);
    println!("isolation_tag: {}", outcome.isolation_tag);
    if let Some(group) = &outcome.served_group {
        println!("served_group: {group}");
    }
    if let Some(fp) = &outcome.served_slot_fingerprint {
        println!("served_slot_fingerprint: {fp}");
    }
    println!(
        "usage: prompt_tokens={} completion_tokens={} reasoning_tokens={} total_tokens={} estimated_fallback={}",
        outcome.usage.prompt_tokens,
        outcome.usage.completion_tokens,
        outcome.usage.reasoning_tokens,
        outcome.usage.total_tokens,
        outcome.usage.estimated_fallback
    );
    if let Some(receipt) = outcome.consumption_receipt {
        println!("receipt_id: {}", format_receipt_id_hex(receipt.receipt_id));
        println!(
            "ledger_cumulative: seq={} total_tokens={} api_calls={}",
            receipt.sequence, receipt.cumulative_total_tokens, receipt.cumulative_api_calls
        );
    }
    print_chat_reply_section(&outcome.content, outcome.reasoning_content.as_deref());
    Ok(())
}

fn print_chat_reply_section(content: &str, reasoning_content: Option<&str>) {
    println!("--- reply ---");
    if content.trim().is_empty()
        && let Some(reasoning) = reasoning_content.map(str::trim).filter(|s| !s.is_empty())
    {
        println!(
            "[note: visible reply empty because max_tokens was exhausted during reasoning_content; increase --max-tokens (e.g. 1024) for final answer]"
        );
        println!("--- reasoning_excerpt ---");
        println!(
            "{}",
            truncate_chars_with_ellipsis(reasoning, REASONING_EXCERPT_MAX_CHARS)
        );
    } else {
        println!("{content}");
    }
}

fn truncate_chars_with_ellipsis(text: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return out;
        };
        out.push(ch);
    }
    if chars.next().is_some() {
        out.push_str("...");
    }
    out
}

async fn run_intent(
    query: &str,
    locale: &str,
    model: Option<&str>,
    root: Option<&Path>,
    pool_config: Option<&Path>,
) -> Result<(), String> {
    let shim = LlmEnforcementShim::discover(pool_config).map_err(|e| e.to_string())?;
    let resource_holder = open_optional_resource_root(root).map_err(|e| e.to_string())?;
    let compiled = if let Some(opened) = &resource_holder {
        shim.compile_nl_intent_with_model(Some(opened.as_context()), query, locale, model)
            .await
            .map_err(|e| e.to_string())?
    } else {
        shim.compile_nl_intent_with_model(None, query, locale, model)
            .await
            .map_err(|e| e.to_string())?
    };

    println!("{}", compiled.to_json_value().to_json_pretty());
    Ok(())
}

struct OpenedResourceBudgetRoot {
    resource: ResourceAuthority,
    account_id: nlos_types::ResourceAccountId,
    operation_id: nlos_types::OperationId,
    idempotency_key: nlos_types::IdempotencyKey,
    now_ms: u64,
}

impl OpenedResourceBudgetRoot {
    const fn as_context(&self) -> MeteredBudgetContext<'_> {
        MeteredBudgetContext {
            resource: &self.resource,
            account_id: self.account_id,
            operation_id: self.operation_id,
            idempotency_key: self.idempotency_key,
            now_ms: self.now_ms,
        }
    }
}

fn open_optional_resource_root(
    root: Option<&Path>,
) -> Result<Option<OpenedResourceBudgetRoot>, LlmDriverError> {
    let Some(state_dir) = root else {
        return Ok(None);
    };
    let resource_dir = state_dir.join("resource");
    let resource = ResourceAuthority::open(resource_dir)?;
    let wall_ms = current_wall_ms();
    let budget = ensure_default_trial_llm_budget(&resource, wall_ms)?;
    let now_ms = wall_ms.max(budget.updated_at_ms);

    let pid_bytes = std::process::id().to_be_bytes();
    let nanos_bytes = current_epoch_nanos().to_be_bytes();
    let seq_bytes = budget.consumed_api_calls.saturating_add(1).to_be_bytes();
    let op_id = derive_operation_id(
        b"nlos/llm-pool-cli/operation/v1",
        &[&pid_bytes, &nanos_bytes, &seq_bytes],
    );
    let idem_key = derive_idempotency_key(
        b"nlos/llm-pool-cli/consume-idem/v1",
        &[&pid_bytes, &nanos_bytes, &seq_bytes],
    );
    Ok(Some(OpenedResourceBudgetRoot {
        resource,
        account_id: budget.account_id,
        operation_id: op_id,
        idempotency_key: idem_key,
        now_ms,
    }))
}

fn current_epoch_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

fn print_usage() {
    println!(
        "nlos-llm-pool-cli: local KeyPool status, metered chat, and NL intent compiler\n\
         \n\
         USAGE:\n\
           nlos-llm-pool-cli status [--pool-config <PATH>]\n\
           nlos-llm-pool-cli chat --prompt <TEXT> [--model <MODEL>] [--max-tokens <N>] [--session <ID>] [--root <STATE_DIR>] [--pool-config <PATH>]\n\
           nlos-llm-pool-cli intent --query <TEXT> [--locale zh-CN|en-US] [--model <MODEL>] [--root <STATE_DIR>] [--pool-config <PATH>]"
    );
}
