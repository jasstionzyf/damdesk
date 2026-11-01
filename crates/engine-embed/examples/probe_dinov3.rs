//! DINOv3 ONNX 探针：输入/输出契约 + f32 试跑。
//! 用法：DAM_TEST_MODELS_DEDUP=/models/dinov3-vits16 cargo run -p engine-embed --example probe_dinov3
use ort::session::Session;
use ort::value::Tensor;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir =
        std::env::var("DAM_TEST_MODELS_DEDUP").unwrap_or_else(|_| "/models/dinov3-vits16".into());
    let mut s = Session::builder()?.commit_from_file(format!("{dir}/model.onnx"))?;
    for i in s.inputs.iter() {
        println!("IN  {} -> {:?}", i.name, i.input_type);
    }
    for o in s.outputs.iter() {
        println!("OUT {} -> {:?}", o.name, o.output_type);
    }
    for size in [224usize, 256] {
        let data = vec![0.5f32; 3 * size * size];
        let t = Tensor::from_array(([1usize, 3, size, size], data.into_boxed_slice()))?;
        match s.run(ort::inputs!["pixel_values" => t]) {
            Ok(out) => {
                for (name, v) in out.iter() {
                    if let Ok(a) = v.try_extract_array::<f32>() {
                        println!(
                            "[{size}] OUT {name} shape {:?} head {:?}",
                            a.shape(),
                            a.iter().take(3).collect::<Vec<_>>()
                        );
                    }
                }
            }
            Err(e) => println!("[{size}] RUN ERR {e}"),
        }
    }
    Ok(())
}
