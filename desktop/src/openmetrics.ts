// W32-E 资源监控:OpenMetrics 文本的严格消费端解析。
//
// 输入是 `nlos_system_control::openmetrics::OpenMetricsRenderer::render` 的
// 确定性输出(`# TYPE <name> counter|gauge` 族头 + 无标签或带标签的十进制
// 样本行,LF 结尾)。解析纪律与 W32-D 数据诚实同源:只结构化渲染文本中
// 真实存在的族/样本;无法识别的行原样归入 `unparsedLines`,绝不发明、
// 绝不静默丢弃。指标目录以仓库恢复指标目录为准(三域 26 族 + worker
// 生命周期),未知族名照常显示并按前缀归域,前缀未知的归入独立分组。

export interface MetricSample {
  labels: Readonly<Record<string, string>>;
  value: string;
}

export interface MetricFamily {
  name: string;
  kind: "counter" | "gauge";
  samples: MetricSample[];
}

export interface ParsedOpenMetrics {
  families: MetricFamily[];
  unparsedLines: string[];
}

export type RecoveryDomainId = "artifact" | "semantic" | "resource";

const TYPE_LINE = /^# TYPE ([a-zA-Z_:][a-zA-Z0-9_:]*) (counter|gauge)$/;
const SAMPLE_LINE = /^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{.*\})? (-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)$/;
const LABEL_PAIR = /^([a-zA-Z_][a-zA-Z0-9_]*)="([^"\\]*)"$/.source;

function parseLabels(raw: string): Record<string, string> | null {
  if (raw.length === 0) {
    return {};
  }
  const labels: Record<string, string> = {};
  const pair = new RegExp(LABEL_PAIR);
  for (const part of raw.split(",")) {
    const match = pair.exec(part.trim());
    if (match === null) {
      return null;
    }
    labels[match[1]] = match[2];
  }
  return labels;
}

export function parseOpenMetricsText(text: string): ParsedOpenMetrics {
  const families: MetricFamily[] = [];
  const unparsedLines: string[] = [];
  let current: MetricFamily | null = null;
  for (const line of text.split("\n")) {
    if (line.length === 0) {
      continue;
    }
    const typeMatch = TYPE_LINE.exec(line);
    if (typeMatch !== null) {
      current = {
        name: typeMatch[1],
        kind: typeMatch[2] as MetricFamily["kind"],
        samples: [],
      };
      families.push(current);
      continue;
    }
    const sampleMatch = SAMPLE_LINE.exec(line);
    if (sampleMatch !== null && current !== null) {
      const labels = parseLabels(
        sampleMatch[2] === undefined ? "" : sampleMatch[2].slice(1, -1),
      );
      if (labels !== null) {
        current.samples.push({ labels, value: sampleMatch[3] });
        continue;
      }
    }
    unparsedLines.push(line);
  }
  return { families, unparsedLines };
}

const DOMAIN_PREFIXES: ReadonlyArray<[RecoveryDomainId, string]> = [
  ["artifact", "nlos_artifact_recovery_"],
  ["semantic", "nlos_semantic_recovery_"],
  ["resource", "nlos_resource_recovery_"],
];

export function familyDomain(name: string): RecoveryDomainId | null {
  for (const [domain, prefix] of DOMAIN_PREFIXES) {
    if (name.startsWith(prefix)) {
      return domain;
    }
  }
  return null;
}
