//! W32-F / B2-2 集成证据:Application 声明 surface → 窗口呈现最小链的
//! 无头断言(W32-A parity-mode 风格:对命令层呈现数据断言,不开 GUI)。
//!
//! 链条:样例包(带 `surfaces` 声明段的小夹具)→ 签名验签(真实
//! ArtifactStore + IdentityAuthority)→ install(verify-then-commit)→
//! register_surfaces(声明段登记,绑定安装代际与 manifest digest)→
//! desktop 呈现(`open_configured_application_authority` +
//! `present_surfaces_core`——Tauri 命令的同一核心,且以**第二个独立
//! 打开的权威句柄**读回,证明桌面读者进程形态)。
//!
//! 诚实断言:stale 代际不呈现(update 到 gen 2 后旧登记不再出现在可
//! 呈现集)、从未安装的包是类型化 NOT_FOUND、未配置 application_root
//! 是类型化 CONFIG 拒绝(无未接线回退形态)。

#![cfg(feature = "dev-fixture")]

use std::path::PathBuf;

use ed25519_dalek::Signer;
use llmos_desktop_lib::error::ErrorCode;
use llmos_desktop_lib::surfaces::{open_configured_application_authority, present_surfaces_core};
use nlos_application::{
    CompatibilityWindow, InstallApplicationRequest, InstallDecision, PackageSurfaceDeclaration,
    PackageSurfaceKind, RegisterSurfacesDecision, RegisterSurfacesRequest,
    UpdateApplicationRequest, UpdateDecision,
};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
    PackageManifestEntry, ProvenanceSourceTriple, PutRevisionRequest, SignedPackage,
    VerifyPackageRequest, package_manifest_message,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_types::{ApplicationId, ArtifactId, IdempotencyKey, PackageId, PrincipalId};

/// 每个测试独享的一次性根目录(进程内唯一)。
fn temp_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "llmosdt-surface-chain-{name}-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

/// 签名验签栈的最小装配(nlos-application 测试 support 的桌面侧裁剪):
/// 真实 IdentityAuthority + 真实 ArtifactStore,密钥窗口 [0, 10_000)。
struct PackageStack {
    artifacts: ArtifactStore,
    identity: IdentityAuthority,
    key: ed25519_dalek::SigningKey,
    principal: PrincipalId,
    _root: PathBuf,
}

impl PackageStack {
    fn new(name: &str, seed: u8) -> Self {
        let root = temp_root(name);
        let artifacts = ArtifactStore::open(root.join("art")).expect("open artifact store");
        let identity = IdentityAuthority::open(root.join("identity")).expect("open identity");
        let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        let binding = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [seed.wrapping_add(1); 32],
                control_domain_policy_digest: [seed.wrapping_add(2); 32],
                public_key: key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: 10_000,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
                created_at_ms: 0,
            })
            .expect("bootstrap principal")
            .binding();
        Self {
            artifacts,
            identity,
            key,
            principal: binding.principal_id,
            _root: root,
        }
    }

    /// 发布一个 entry 载荷并构建/签名/验签一个单 entry 包(权威验签
    /// 管线,与 CLI `nlos-package verify` 同一条路)。
    fn verify_package(
        &self,
        package_seed: u8,
        version: u64,
        entry_payload: &[u8],
        key: IdempotencyKey,
        verified_at_ms: u64,
    ) -> nlos_artifact::PackageVerificationReceipt {
        let artifact_id = ArtifactId::from_bytes([package_seed; 16]);
        self.artifacts
            .create_artifact(CreateArtifactSpec {
                artifact_id,
                idempotency_key: IdempotencyKey::from_bytes(
                    [(0xa0_u8).wrapping_add(package_seed); 16],
                ),
                content_type: "application/octet-stream".to_string(),
                application_id: Some(ApplicationId::from_bytes(
                    [(0xb0_u8).wrapping_add(package_seed); 16],
                )),
                owner: Some(format!("sample-{package_seed}")),
                created_at_ms: 1_000,
            })
            .expect("create artifact");
        self.artifacts
            .put_revision(PutRevisionRequest {
                artifact_id,
                expected_head_revision: 0,
                bytes: entry_payload,
                created_at_ms: 5_000,
                provenance: ProvenanceSourceTriple {
                    source_a: [0xc0_u8.wrapping_add(package_seed); 16],
                    source_b: [0xd0_u8.wrapping_add(package_seed); 16],
                    source_digest: ContentDigest::of_bytes(entry_payload),
                },
            })
            .expect("put revision");
        let manifest = PackageManifest {
            package_id: PackageId::from_bytes([package_seed; 16]),
            version,
            entries: vec![PackageManifestEntry {
                name: "main".to_string(),
                artifact_id,
                digest: ContentDigest::of_bytes(entry_payload),
                role: PackageEntryRole::Executable,
            }],
        };
        let message = package_manifest_message(&manifest);
        let signed = SignedPackage {
            manifest,
            signer: self.principal,
            signature: self.key.sign(&message).to_bytes(),
        };
        self.artifacts
            .verify_package(
                &self.identity,
                VerifyPackageRequest {
                    signed: &signed,
                    idempotency_key: key,
                    verified_at_ms,
                },
            )
            .expect("verify package")
            .receipt()
            .clone()
    }
}

/// 样例包声明的 surfaces 段:一个窗口主表面 + 一个带 entry 内容引用的
/// 面板(声明数据与包载荷同源,由应用侧持有;登记时绑定 manifest digest)。
fn sample_segment() -> Vec<PackageSurfaceDeclaration> {
    vec![
        PackageSurfaceDeclaration {
            surface_id: [0x01; 16],
            kind: PackageSurfaceKind::Window,
            title: "样板应用主窗口".to_string(),
            entry_name: None,
        },
        PackageSurfaceDeclaration {
            surface_id: [0x02; 16],
            kind: PackageSurfaceKind::Panel,
            title: "运行面板".to_string(),
            entry_name: Some("main".to_string()),
        },
    ]
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// 全链:验签 → install → 登记 → 第二个权威句柄上的桌面呈现。
#[test]
fn declared_surfaces_present_through_the_desktop_command_face() {
    let name = "present";
    let stack = PackageStack::new(name, 0xD1);
    let authority_root = temp_root(&format!("{name}-authority"));
    let authority =
        nlos_application::ApplicationAuthority::open(&authority_root).expect("open authority");

    let verification = stack.verify_package(
        0xD2,
        1,
        b"sample surface package payload v1",
        IdempotencyKey::from_bytes([0x91; 16]),
        6_000,
    );
    let installation = match authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verification.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
                installed_at_ms: 7_000,
            },
        )
        .expect("install")
    {
        InstallDecision::Installed(receipt) => receipt,
        InstallDecision::Replayed(receipt) => panic!("fresh key cannot replay, got {receipt:?}"),
    };

    let registered = match authority
        .register_surfaces(&RegisterSurfacesRequest {
            package_id: verification.package_id,
            declared_manifest_digest: installation.package_manifest_digest,
            surfaces: sample_segment(),
            registrant_principal: installation.installer_principal,
            idempotency_key: IdempotencyKey::from_bytes([0x93; 16]),
            registered_at_ms: 7_500,
        })
        .expect("register surfaces")
    {
        RegisterSurfacesDecision::Registered(receipts) => receipts,
        RegisterSurfacesDecision::Replayed(receipts) => {
            panic!("fresh key cannot replay, got {receipts:?}")
        }
    };
    assert_eq!(registered.len(), 2);

    // 桌面读者:第二个独立打开的权威句柄 + 呈现核心(Tauri 命令的同
    // 一组合)。
    let desktop = open_configured_application_authority(authority_root.to_str())
        .expect("desktop reader opens a second authority handle");
    let presentation =
        present_surfaces_core(&desktop, verification.package_id).expect("present surfaces");

    assert_eq!(
        presentation.application_id_hex,
        lower_hex(installation.application_id.as_bytes())
    );
    assert_eq!(
        presentation.package_id_hex,
        lower_hex(verification.package_id.as_bytes())
    );
    assert_eq!(presentation.application_generation, 1);
    assert_eq!(presentation.status, "installed");
    assert_eq!(
        presentation.package_manifest_digest_hex,
        lower_hex(installation.package_manifest_digest.as_bytes()),
        "呈现事实绑定安装内容的 manifest digest"
    );

    // 窗口呈现:声明序、逐字段 == durable 声明事实,无发明字段。
    assert_eq!(presentation.presentable_surfaces.len(), 2);
    let window = &presentation.presentable_surfaces[0];
    assert_eq!(window.surface_id_hex, lower_hex(&[0x01; 16]));
    assert_eq!(window.kind, "window");
    assert_eq!(window.title, "样板应用主窗口");
    assert_eq!(window.entry_name, None);
    assert_eq!(window.registration_key_hex, lower_hex(&[0x93; 16]));
    assert_eq!(window.registered_at_ms, 7_500);
    let panel = &presentation.presentable_surfaces[1];
    assert_eq!(panel.surface_id_hex, lower_hex(&[0x02; 16]));
    assert_eq!(panel.kind, "panel");
    assert_eq!(panel.title, "运行面板");
    assert_eq!(panel.entry_name.as_deref(), Some("main"));

    // 未配置 application_root:类型化 CONFIG 拒绝(无未接线回退形态)。
    let unwired = match open_configured_application_authority(None) {
        Err(error) => error,
        Ok(_) => panic!("unconfigured application_root must refuse"),
    };
    assert_eq!(unwired.code, ErrorCode::Config);
}

/// 诚实边界:stale 代际不呈现(update 后旧登记仍在权威里但不再是可
/// 呈现集)、重声明后恢复呈现、从未安装的包是类型化 NOT_FOUND。
#[test]
fn stale_generation_and_unknown_package_present_honestly() {
    let name = "stale";
    let stack = PackageStack::new(name, 0xD5);
    let authority_root = temp_root(&format!("{name}-authority"));
    let authority =
        nlos_application::ApplicationAuthority::open(&authority_root).expect("open authority");

    let first = stack.verify_package(
        0xD6,
        1,
        b"sample surface package payload v1",
        IdempotencyKey::from_bytes([0x91; 16]),
        6_000,
    );
    let installed = match authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: first.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
                installed_at_ms: 7_000,
            },
        )
        .expect("install")
    {
        InstallDecision::Installed(receipt) => receipt,
        InstallDecision::Replayed(receipt) => {
            panic!("fresh key cannot replay, got {receipt:?}")
        }
    };
    authority
        .register_surfaces(&RegisterSurfacesRequest {
            package_id: first.package_id,
            declared_manifest_digest: installed.package_manifest_digest,
            surfaces: sample_segment(),
            registrant_principal: installed.installer_principal,
            idempotency_key: IdempotencyKey::from_bytes([0x93; 16]),
            registered_at_ms: 7_500,
        })
        .expect("generation-1 registration");

    let desktop =
        open_configured_application_authority(authority_root.to_str()).expect("desktop reader");
    let generation_one =
        present_surfaces_core(&desktop, first.package_id).expect("present generation 1");
    assert_eq!(generation_one.presentable_surfaces.len(), 2);

    // 内容更新:同 major 新版本,代际 1 → 2。
    let second = stack.verify_package(
        0xD6,
        2,
        b"sample surface package payload v1",
        IdempotencyKey::from_bytes([0x95; 16]),
        8_000,
    );
    match authority
        .update_application(
            &stack.artifacts,
            UpdateApplicationRequest {
                package_id: first.package_id,
                package_verification_receipt_id: second.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x96; 16]),
                updated_at_ms: 8_500,
                compatibility_window: CompatibilityWindow::SameMajor,
            },
        )
        .expect("update")
    {
        UpdateDecision::Updated(receipt) => assert_eq!(receipt.installation_generation.get(), 2),
        UpdateDecision::Replayed(receipt) => panic!("fresh key cannot replay, got {receipt:?}"),
    }

    // stale 代际不呈现:登记仍是 durable 事实,但可呈现集如实为空。
    let generation_two =
        present_surfaces_core(&desktop, first.package_id).expect("present generation 2");
    assert_eq!(generation_two.application_generation, 2);
    assert_eq!(generation_two.status, "installed");
    assert_eq!(
        generation_two.package_manifest_digest_hex,
        lower_hex(second.manifest_digest.as_bytes())
    );
    assert!(
        generation_two.presentable_surfaces.is_empty(),
        "generation-1 registrations are stale and must not present"
    );
    assert_eq!(
        authority
            .inspect_surfaces(first.package_id)
            .expect("inspect")
            .len(),
        2,
        "the durable declarations remain registered facts"
    );

    // gen 2 重声明(同 surface id,新代际):恢复呈现。
    match authority
        .register_surfaces(&RegisterSurfacesRequest {
            package_id: first.package_id,
            declared_manifest_digest: second.manifest_digest,
            surfaces: vec![PackageSurfaceDeclaration {
                surface_id: [0x01; 16],
                kind: PackageSurfaceKind::Window,
                title: "样板应用主窗口 v2".to_string(),
                entry_name: None,
            }],
            registrant_principal: installed.installer_principal,
            idempotency_key: IdempotencyKey::from_bytes([0x97; 16]),
            registered_at_ms: 9_000,
        })
        .expect("generation-2 re-declaration")
    {
        RegisterSurfacesDecision::Registered(receipts) => assert_eq!(receipts.len(), 1),
        RegisterSurfacesDecision::Replayed(receipts) => {
            panic!("fresh key cannot replay, got {receipts:?}")
        }
    }
    let redeclared =
        present_surfaces_core(&desktop, first.package_id).expect("present after re-declaration");
    assert_eq!(redeclared.presentable_surfaces.len(), 1);
    assert_eq!(
        redeclared.presentable_surfaces[0].title,
        "样板应用主窗口 v2"
    );
    assert_eq!(redeclared.application_generation, 2);

    // 从未安装的包:类型化 NOT_FOUND(事实读回,非错误重试面)。
    let unknown = present_surfaces_core(&desktop, PackageId::from_bytes([0xEE; 16]))
        .expect_err("unknown package must refuse");
    assert_eq!(unknown.code, ErrorCode::NotFound);
}
