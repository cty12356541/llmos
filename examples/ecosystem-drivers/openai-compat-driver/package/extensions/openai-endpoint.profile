# Gateway endpoint profile for openai-compat-driver v1.0.0.
# Declared egress: must be covered by certify.manifest egress-allow.
endpoint-base = https://llm-gateway.openai-compatible.example
chat-completions-path = /v1/chat/completions
credential-custody = enforcement-shim-only
connect-timeout-ms = 5000
max-stream-events = 4096
