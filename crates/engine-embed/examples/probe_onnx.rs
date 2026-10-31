//! ONNX 探针（开发/验收工具）：打印 SigLIP2 两塔的输入输出名与 dtype，并试跑 f32 / f16 两种输入。
//! 用法：DAM_TEST_MODELS=/models/siglip2-base-p224-onnx cargo run -p engine-embed --example probe_onnx
//!
//! 用途：确认 ONNX 图契约（输入名 / 元素类型 / 输出名）与 fp16 dtype 在 CPU EP 上是否可跑。

use ort::session::Session;
use ort::value::{Tensor, ValueType};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::var("DAM_TEST_MODELS")
        .unwrap_or_else(|_| "/models/siglip2-base-p224-onnx".to_string());
    for (file, kind) in [("vision.onnx", "vision"), ("text.onnx", "text")] {
        let path = format!("{dir}/{file}");
        println!("== {path}");
        let mut session = Session::builder()?.commit_from_file(&path)?;
        let mut input_is_f16 = false;
        for i in session.inputs.iter() {
            println!("   in  {} -> {:?}", i.name, i.input_type);
            if let ValueType::Tensor { ty, .. } = &i.input_type {
                println!("       tensor ty = {ty:?}");
                input_is_f16 = format!("{ty:?}").contains("Float16");
            }
        }
        for o in session.outputs.iter() {
            println!("   out {} -> {:?}", o.name, o.output_type);
        }
        println!("   input_is_f16 = {input_is_f16}");

        // f32 试跑
        let run = if kind == "vision" {
            let data = vec![0.5f32; 3 * 224 * 224];
            let t = Tensor::from_array(([1usize, 3, 224, 224], data.into_boxed_slice()))?;
            session.run(ort::inputs!["pixel_values" => t])
        } else {
            let ids: Vec<i64> = vec![2, 100, 200, 1];
            let t = Tensor::from_array(([1usize, 4], ids.into_boxed_slice()))?;
            session.run(ort::inputs!["input_ids" => t])
        };
        report("f32", run);
    }
    Ok(())
}

fn report(tag: &str, run: ort::error::Result<ort::session::SessionOutputs<'_>>) {
    match run {
        Ok(out) => {
            for k in [
                "pooler_output",
                "last_hidden_state",
                "image_embeds",
                "text_embeds",
            ] {
                if let Some(v) = out.get(k) {
                    match v.try_extract_array::<f32>() {
                        Ok(a) => {
                            let s = a.shape().to_vec();
                            let head: Vec<f32> = a.iter().copied().take(3).collect();
                            println!("   [{tag}] ran {k} shape {s:?} head {head:?}");
                        }
                        Err(e) => println!("   [{tag}] ran {k} (extract f32 failed: {e})"),
                    }
                }
            }
        }
        Err(e) => println!("   [{tag}] RUN ERR {e}"),
    }
}
