# W82-B-CONTRACT / Attempt20261116-1 制品

Owner 与结论入口为 [Evidence](../w82-b-contract-20261116.md)。输入绑定两个不同的 Git revision，见 `receipt.json`；`artifact-digests.json` 逐文件绑定最终制品字节。这里没有真实用户载荷、凭据或运行密钥。

2026-11-16补充裁定后的候选在 candidate-post-ruling/，新增前到该候选的报告为 compat-diff-post-ruling.json，B1到该候选为 compat-diff-b1-to-post-ruling.json。原 candidate/、compat-diff.json 是实施前B1快照，保留用于审计。candidate-source-digests.json 绑定最终代码/设计；*-post-ruling 日志与 receipt.json.post_ruling 记录候选实际验证，生成原门的dirty拒绝未被隐藏。

`compat-diff.json` 和 `compat-input-digests.json` 保留原运行输出（其输入路径含执行时的本机 TEMP 目录）。本目录的 `baseline/`、`candidate/` 是相同输入的可移植归档。从仓库根目录可重放：

```powershell
python tools/sabi-compat-diff/sabi_compat_diff.py docs/evidence/stage-e/w82-b-contract-20261116-artifacts/baseline/schema/nlos/sabi/v1/deprecation-manifest.yaml docs/evidence/stage-e/w82-b-contract-20261116-artifacts/candidate/schema/nlos/sabi/v1/deprecation-manifest.yaml --baseline-proto-dir docs/evidence/stage-e/w82-b-contract-20261116-artifacts/baseline/schema/nlos/sabi/v1 --candidate-proto-dir docs/evidence/stage-e/w82-b-contract-20261116-artifacts/candidate/schema/nlos/sabi/v1 --json
python tools/check_golden_immutability.py --check-git-diff '6e9f0a9f^'
cargo test -p nlos-schema
npm run schema:lint
npm run schema:check-generated
python -m unittest discover -s tests/conformance/schema -p 'test_*.py'
node --import tsx docs/evidence/stage-e/w82-b-contract-20261116-artifacts/measure-blob-boundaries.mjs
```

编码诊断使用项目现有 `npm ci` 依赖。它在 JSON 中验证精确65536/65537边界、候选65408块的最大78字节开销，以及非canonical响应扩展导致超界的反例；它不执行 Blob 服务，也不替代 B4 接受测试。

本机 Go 与 .NET SDK 不可用（`environment.json`、`csharp-probe.log`）。同一 SHA 的 GitHub Actions 原始日志经 `gh run view 43576575485 --log`、`gh run view 43649840304 --job 132615620831 --log` 获取。`ci-probe-excerpts.log` 保留原文件名、原行号和探针非零用例结果；最新 run 各平台步骤结果在 `ci-latest-jobs.json`。最新整条 run 的 Windows workspace 失败不被探针成功掩盖。

日志与 JSON 中的本机路径、CI job ID 都是定位信息；schema `candidate/` 表示本次只读契约复验的候选快照，未包含未来尚待裁定的 Blob 修改。

归档字节保真修复（2026-11-16，fix round 1）：本目录全部文件在 .gitattributes 中精确设为 -text -diff -merge（binary属性），禁止 core.autocrlf 在 Git clean/checkout 时改写采集字节，并避免原生 Git 格式门把原始CRLF或工具尾空行当作源码格式错误。被 artifact-digests.json 引用的主 Evidence 固定 text eol=lf；candidate-source-digests.json 中两个 Rust 文件与设计文档也只按这三个精确路径固定 text eol=lf，其余已由既有 schema/gen/golden 规则覆盖，保持正常文本diff。没有统一转换或trim原日志，也没有重算日志摘要来掩盖采集内容变化。

复验使用 GIT_INDEX_FILE 指向临时索引（不修改正常暂存区），git read-tree --empty 后仅 add .gitattributes、70项制品、摘要清单、主Evidence与9项source；比较 git hash-object --no-filters 与默认过滤后的对象，随后分别以 git -c core.autocrlf=true/false/input checkout-index --all --prefix=<独立临时目录>/ 导出并逐字节验证原 SHA-256/长度，同时要求原生 git diff --cached --check 退出0。该验证不创建提交，不重跑或改变 schema 功能。README 的说明增量是本轮唯一变动的已登记制品，其摘要随 artifact-digests.json 更新；原采集日志和所有源码摘要保持原值。
