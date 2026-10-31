Model weights are NOT stored in this repository.

The bundled retrieval tower (SigLIP2 base text + vision + tokenizer) is staged here
before packaging, so that sources stay small and weights stay under their own terms:

  models/siglip2-base-p224-onnx/vision.onnx
  models/siglip2-base-p224-onnx/text.onnx
  models/siglip2-base-p224-onnx/tokenizer.json

Stage them from your model cache (byte sizes and sha256 are declared in
crates/engine-embed/src/model_store.rs):

  cp -r <models_root>/models/siglip2-base-p224-onnx/* \
        crates/app-tauri/resources/models/siglip2-base-p224-onnx/

Set DAM_RELEASE_GATE=1 when packaging: the bundle gate then verifies that only this
tower is bundled and that every file matches the declared bytes + sha256.

Optional enhancement models (u2net, PP-OCRv6, Real-ESRGAN, LaMa) are never bundled;
they are fetched on demand by the app (Settings -> Models) or from
damdesk-models-extras-v1.zip.
