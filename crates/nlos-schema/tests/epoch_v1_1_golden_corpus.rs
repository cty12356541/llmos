//! W82-B1 integration test pinning `schema/golden/epochs/v1.1/` against
//! `nlos-schema`: the three SABI v1.1 thin-waist channels (LlmGateway,
//! ToolSandbox, BlobReference) must decode, adjudicate at
//! [`VersionRoute::NativeCurrentEpoch`], and re-encode byte-for-byte
//! deterministically across all 6 epoch v1.1 golden vectors, including the
//! unknown-noncritical-extension round-trip vectors.

use std::fs;
use std::path::{Path, PathBuf};

use nlos_schema::{
    SABI_BLOB_REFERENCE_SCHEMA, SABI_LLM_GATEWAY_SCHEMA, SABI_TOOL_SANDBOX_SCHEMA,
    VersionCompatibilityPolicy, VersionRoute, blob_reference_schema_identity,
    canonical_deprecation_manifest, decode_invoke_tool_request, decode_list_tools_request,
    decode_llm_chat_request, decode_mint_reference_request, decode_query_budget_request,
    decode_read_reference_request, encode_invoke_tool_request, encode_list_tools_request,
    encode_llm_chat_request, encode_mint_reference_request, encode_query_budget_request,
    encode_read_reference_request, llm_gateway_schema_identity, registry_frozen,
    tool_sandbox_schema_identity,
};

fn epoch_v1_1_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schema/golden/epochs/v1.1")
}

fn decode_hex_file(path: &Path) -> Vec<u8> {
    let raw = fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    assert!(raw.ends_with('\n'), "{} must end with LF", path.display());
    let trimmed = raw.trim_end_matches('\n');
    assert_eq!(trimmed.len() % 2, 0, "{} odd hex length", path.display());
    (0..trimmed.len())
        .step_by(2)
        .map(|idx| {
            u8::from_str_radix(&trimmed[idx..idx + 2], 16)
                .unwrap_or_else(|err| panic!("invalid hex in {}: {err}", path.display()))
        })
        .collect()
}

#[test]
fn epoch_v1_1_corpus_llm_gateway_tool_sandbox_and_blob_reference_round_trip() {
    let dir = epoch_v1_1_dir();
    let policy = VersionCompatibilityPolicy::default();

    // The v1.1 epoch directory covers exactly the three additive channels; the
    // canonical manifest itself carries all 12 frozen channels.
    let manifest = canonical_deprecation_manifest().expect("canonical deprecation manifest");
    assert_eq!(manifest.channels.len(), 12);
    for channel in [
        SABI_LLM_GATEWAY_SCHEMA,
        SABI_TOOL_SANDBOX_SCHEMA,
        SABI_BLOB_REFERENCE_SCHEMA,
    ] {
        assert_eq!(
            registry_frozen(channel),
            Some(true),
            "{channel} must be frozen"
        );
    }

    // 1. LlmGateway typical positive request
    let chat_wire =
        decode_hex_file(&dir.join("nlos.sabi.LlmGateway.LlmChatRequest-v1.1.hex"));
    let chat = decode_llm_chat_request(&chat_wire).expect("decode LlmChatRequest");
    assert_eq!(
        policy
            .adjudicate_schema_identity(chat.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(chat.model, "glm-4.6");
    assert_eq!(encode_llm_chat_request(&chat).unwrap(), chat_wire);

    // 2. LlmGateway unknown non-critical extension round-trip
    let budget_wire = decode_hex_file(
        &dir.join("nlos.sabi.LlmGateway.QueryBudgetRequest-unknown-noncritical-v1.1.hex"),
    );
    let budget = decode_query_budget_request(&budget_wire).expect("decode QueryBudgetRequest");
    let budget_identity = budget.schema.as_ref().unwrap();
    assert_eq!(
        policy.adjudicate_schema_identity(budget_identity).unwrap().route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(budget_identity.non_critical_extension_ids, vec![42, 7001]);
    assert_eq!(
        encode_query_budget_request(&budget).unwrap(),
        budget_wire,
        "unknown non-critical extension ids must survive the bounded round trip"
    );

    // 3. ToolSandbox typical positive request
    let invoke_wire =
        decode_hex_file(&dir.join("nlos.sabi.ToolSandbox.InvokeToolRequest-v1.1.hex"));
    let invoke = decode_invoke_tool_request(&invoke_wire).expect("decode InvokeToolRequest");
    assert_eq!(
        policy
            .adjudicate_schema_identity(invoke.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(invoke.tool, "web.fetch");
    assert_eq!(encode_invoke_tool_request(&invoke).unwrap(), invoke_wire);

    // 4. ToolSandbox unknown non-critical extension round-trip
    let list_wire = decode_hex_file(
        &dir.join("nlos.sabi.ToolSandbox.ListToolsRequest-unknown-noncritical-v1.1.hex"),
    );
    let list = decode_list_tools_request(&list_wire).expect("decode ListToolsRequest");
    assert_eq!(
        list.schema.as_ref().unwrap().non_critical_extension_ids,
        vec![42, 7001]
    );
    assert_eq!(encode_list_tools_request(&list).unwrap(), list_wire);

    // 5. BlobReference typical positive request
    let mint_wire =
        decode_hex_file(&dir.join("nlos.sabi.BlobReference.MintReferenceRequest-v1.1.hex"));
    let mint = decode_mint_reference_request(&mint_wire).expect("decode MintReferenceRequest");
    assert_eq!(
        policy
            .adjudicate_schema_identity(mint.schema.as_ref().unwrap())
            .unwrap()
            .route,
        VersionRoute::NativeCurrentEpoch
    );
    assert_eq!(mint.file_digest_sha256.len(), 32);
    assert_eq!(encode_mint_reference_request(&mint).unwrap(), mint_wire);

    // 6. BlobReference unknown non-critical extension round-trip
    let read_wire = decode_hex_file(
        &dir.join("nlos.sabi.BlobReference.ReadReferenceRequest-unknown-noncritical-v1.1.hex"),
    );
    let read = decode_read_reference_request(&read_wire).expect("decode ReadReferenceRequest");
    assert_eq!(
        read.schema.as_ref().unwrap().non_critical_extension_ids,
        vec![42, 7001]
    );
    assert_eq!(read.reference_id.len(), 16);
    assert_eq!(encode_read_reference_request(&read).unwrap(), read_wire);
}

#[test]
fn epoch_v1_1_schema_identity_helpers_match_the_canonical_registry() {
    let llm = llm_gateway_schema_identity();
    assert_eq!(llm.name, SABI_LLM_GATEWAY_SCHEMA);
    assert_eq!((llm.major, llm.minor), (1, 0));
    let tool = tool_sandbox_schema_identity();
    assert_eq!(tool.name, SABI_TOOL_SANDBOX_SCHEMA);
    assert_eq!((tool.major, tool.minor), (1, 0));
    let blob = blob_reference_schema_identity();
    assert_eq!(blob.name, SABI_BLOB_REFERENCE_SCHEMA);
    assert_eq!((blob.major, blob.minor), (1, 0));
}
