#!/usr/bin/env bash
# Downloads the on-device speech models Tsetsen uses, about 3 GB, into
# $TSETSEN_MODELS_DIR (default ~/.tsetsen/models). Files already present are
# skipped, so it is safe to run again. The thinker is pulled with Ollama
# separately: ollama pull qwen3.5:9b-mlx
set -euo pipefail

models="${TSETSEN_MODELS_DIR:-$HOME/.tsetsen/models}"

fetch() { # url destination
  if [[ -s "$2" ]]; then
    return
  fi
  mkdir -p "$(dirname "$2")"
  echo "  $(basename "$2")"
  curl -fL --retry 3 --progress-bar -o "$2.part" "$1"
  mv "$2.part" "$2"
}

echo "Speech-to-text: Parakeet TDT 0.6B v3 (CC-BY-4.0), ~2.4 GB"
hf=https://huggingface.co/altunenes/parakeet-rs/resolve/main/parakeet-tdt-0.6b-v3
for f in encoder-model.onnx encoder-model.onnx.data decoder_joint-model.onnx vocab.txt; do
  fetch "$hf/$f" "$models/parakeet-tdt-0.6b-v3/$f"
done

echo "Voice: Supertonic 3 (OpenRAIL-M), ~400 MB"
hf=https://huggingface.co/supertone-oss-archive/supertonic-3/resolve/main
for f in LICENSE onnx/{duration_predictor,text_encoder,vector_estimator,vocoder}.onnx \
         onnx/{tts,unicode_indexer}.json voice_styles/{F,M}{1,2,3,4,5}.json; do
  fetch "$hf/$f" "$models/supertonic-3/$f"
done

echo "Voice activity: Silero VAD (MIT), 2 MB"
fetch https://github.com/snakers4/silero-vad/raw/master/src/silero_vad/data/silero_vad.onnx \
  "$models/silero-vad/silero_vad.onnx"

echo "Done: $models"
