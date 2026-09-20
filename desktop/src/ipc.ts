import { invoke } from "@tauri-apps/api/core";

import type { ConfigDto, ParityDto, ReceiptDto } from "./types";

export async function getConfig(): Promise<ConfigDto> {
  return invoke("get_config");
}

export interface SetConfigInput {
  socketPath?: string | null;
  principalHex?: string | null;
  keyFile?: string | null;
  cliSocket?: string | null;
  cliPath?: string | null;
}

export async function setConfig(input: SetConfigInput): Promise<ConfigDto> {
  return invoke("set_config", { input });
}

export async function inspectHealth(): Promise<ReceiptDto> {
  return invoke("inspect_health");
}

export async function inspectSemanticHealth(): Promise<ReceiptDto> {
  return invoke("inspect_semantic_health");
}

export async function exportMetrics(): Promise<ReceiptDto> {
  return invoke("export_metrics");
}

export async function exportSemanticMetrics(): Promise<ReceiptDto> {
  return invoke("export_semantic_metrics");
}

export async function inspectTask(planIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_task", { planIdHex });
}

export async function inspectProcess(processIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_process", { processIdHex });
}

export async function inspectResource(reservationIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_resource", { reservationIdHex });
}

export async function parityCheck(
  operation: string,
  targetHex?: string | null,
): Promise<ParityDto> {
  return invoke("parity_check", { operation, targetHex: targetHex ?? null });
}
