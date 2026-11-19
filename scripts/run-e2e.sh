#!/usr/bin/env bash
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

MODE="full"
RUN_CODEX=1

for arg in "$@"; do
  case "$arg" in
    --probe) MODE="probe" ;;
    --full) MODE="full" ;;
    --no-codex) RUN_CODEX=0 ;;
    -h|--help)
      cat <<'EOF'
Usage: scripts/run-e2e.sh [--probe|--full] [--no-codex]

  --probe     Run deterministic GUI tests + real engine capability & live-caption probes.
  --full      Run all local real-engine, real-GUI, Codex (if signed in), and cloud E2E tests (default).
  --no-codex  Skip Codex CLI live tests.
EOF
      exit 0
      ;;
    *)
      echo "Unknown option: $arg" >&2
      exit 2
      ;;
  esac
done

resolve_engine() {
  local name="$1"
  local env_val="$2"
  if [[ -n "$env_val" && -x "$env_val" ]]; then
    printf '%s\n' "$env_val"
    return 0
  fi
  for cand in \
    "$ROOT/target/engines/Debug/$name/$name" \
    "$ROOT/target/debug/FristonDev.app/Contents/Resources/engines/$name/$name" \
    "$ROOT/target/engines/Release/$name/$name"
  do
    if [[ -x "$cand" ]]; then
      printf '%s\n' "$cand"
      return 0
    fi
  done
  return 1
}

MEETINGD_BIN="$(resolve_engine mm-meetingd "${MM_E2E_ENGINE:-}")" || {
  echo "ERROR: mm-meetingd binary not found. Run 'make engines' or 'scripts/dev-app.sh build' first." >&2
  exit 1
}
EMBEDD_BIN="$(resolve_engine mm-embedd "${MM_E2E_EMBED_ENGINE:-}")" || {
  echo "ERROR: mm-embedd binary not found. Run 'make engines' or 'scripts/dev-app.sh build' first." >&2
  exit 1
}

export MM_E2E_ENGINE="$MEETINGD_BIN"
export MM_E2E_EMBED_ENGINE="$EMBEDD_BIN"

echo "== Meeting Master E2E Runner (mode=$MODE) =="
echo "MM_E2E_ENGINE=$MM_E2E_ENGINE"
echo "MM_E2E_EMBED_ENGINE=$MM_E2E_EMBED_ENGINE"
echo ""

declare -a NAMES=()
declare -a RESULTS=()
FAILURES=0

run_case() {
  local label="$1"
  shift
  echo "----------------------------------------------------------------"
  echo "[RUN] $label"
  echo "CMD: $*"
  echo "----------------------------------------------------------------"
  if "$@"; then
    NAMES+=("$label")
    RESULTS+=("PASS")
  else
    NAMES+=("$label")
    RESULTS+=("FAIL")
    FAILURES=$((FAILURES + 1))
  fi
}

skip_case() {
  local label="$1"
  local reason="$2"
  echo "[SKIP] $label ($reason)"
  NAMES+=("$label")
  RESULTS+=("SKIP ($reason)")
}

# Phase 1: Deterministic GPUI GUI E2E suite
run_case "gui_tests (deterministic headless GPUI)" \
  cargo test -p mm-app gui_tests -- --nocapture

# Phase 2: Real engine capability & live-captions probes
run_case "e2e_probe_real_capabilities" \
  cargo test -p mm-app e2e_probe_real_capabilities -- --ignored --nocapture

run_case "e2e_real_live_captions_local_voiceprint_spatial_and_fallback" \
  cargo test -p mm-app e2e_real_live_captions_local_voiceprint_spatial_and_fallback -- --ignored --nocapture

if [[ "$MODE" == "full" ]]; then
  # Phase 3: Real offline refine + local LLM minutes + EmbeddingGemma semantic search + fallback
  run_case "e2e_cloud_refine_fallback_to_local_pipeline" \
    cargo test -p mm-app e2e_cloud_refine_fallback_to_local_pipeline -- --ignored --nocapture

  run_case "e2e_real_local_refine_and_local_llm_minutes_offline" \
    cargo test -p mm-app e2e_real_local_refine_and_local_llm_minutes_offline -- --ignored --nocapture

  # Phase 4: Real engines driving real GPUI Workbench & FloatingCaptions UI
  run_case "gui_real_live_captions_to_workbench_and_floating_e2e" \
    cargo test -p mm-app gui_real_live_captions_to_workbench_and_floating_e2e -- --ignored --nocapture

  run_case "gui_real_embedd_semantic_search_and_local_llm_minutes_e2e" \
    cargo test -p mm-app gui_real_embedd_semantic_search_and_local_llm_minutes_e2e -- --ignored --nocapture

  # Phase 5: Local Codex CLI E2E (when signed in and not disabled)
  if [[ "$RUN_CODEX" -eq 1 ]] && command -v codex >/dev/null 2>&1 && codex login status >/dev/null 2>&1; then
    run_case "gui_live_codex_unified_audio_to_library_e2e" \
      cargo test -p mm-app gui_live_codex_unified_audio_to_library_e2e -- --ignored --nocapture
  else
    skip_case "gui_live_codex_unified_audio_to_library_e2e" "codex disabled or not signed in"
  fi

  # Phase 6: Cloud live & refine E2E (when env API keys are set)
  if [[ -n "${OPENAI_API_KEY:-}" ]]; then
    run_case "openai_asr_live (mm-llm)" \
      cargo test -p mm-llm --test openai_asr_live -- --ignored --nocapture
    run_case "openai_live (mm-service)" \
      cargo test -p mm-service --test openai_live -- --ignored --nocapture
    run_case "e2e_openai_live_and_refine_pipeline" \
      cargo test -p mm-app e2e_openai_live_and_refine_pipeline -- --ignored --nocapture
  else
    skip_case "OpenAI cloud live E2E suite" "OPENAI_API_KEY not set"
  fi

  if [[ -n "${GEMINI_API_KEY:-}" ]]; then
    run_case "gemini_asr_live (mm-llm)" \
      cargo test -p mm-llm --test gemini_asr_live -- --ignored --nocapture
    run_case "gemini_live (mm-service)" \
      cargo test -p mm-service --test gemini_live -- --ignored --nocapture
    run_case "e2e_gemini_live_and_refine_pipeline" \
      cargo test -p mm-app e2e_gemini_live_and_refine_pipeline -- --ignored --nocapture
  else
    skip_case "Gemini cloud live E2E suite" "GEMINI_API_KEY not set"
  fi
fi

echo ""
echo "==================== E2E SUMMARY ===================="
for i in "${!NAMES[@]}"; do
  printf "  %-60s %s\n" "${NAMES[$i]}" "${RESULTS[$i]}"
done
echo "====================================================="

if [[ "$FAILURES" -ne 0 ]]; then
  echo "$FAILURES E2E step(s) failed." >&2
  exit 1
fi
