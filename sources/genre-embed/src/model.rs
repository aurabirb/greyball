//! Download/cache + load of Essentia's discogs-effnet ONNX model, and running it over patches
//! of mel-spectrogram frames.
//!
//! discogs-effnet (Music Technology Group, Universitat Pompeu Fabra) is released under
//! **CC BY-NC-SA 4.0** — non-commercial, share-alike. See
//! <https://essentia.upf.edu/models.html> for the model card and license text.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tract_onnx::prelude::*;

use crate::mel::BANDS;

const MODEL_URL: &str = "https://essentia.upf.edu/models/feature-extractors/discogs-effnet/discogs-effnet-bsdynamic-1.onnx";
const MODEL_FILE: &str = "discogs-effnet-bsdynamic-1.onnx";
const MODEL_SHA256: &str = "a280825b334797cf677939db8cd5762c0392aedd0ca6415dbc1cd083f045e43c";

/// Output tensor to select: the 1280-dim embedding layer, not the default 400-way genre head.
const OUTPUT_LABEL: &str = "embeddings";
pub const DIMS: usize = 1280;

/// The model's own recommended patching of the mel-frame stream for inference.
pub const PATCH_FRAMES: usize = 128;
pub const PATCH_HOP: usize = 64;

pub type Plan = std::sync::Arc<TypedRunnableModel>;

fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Ensures the model file is present under `data_dir/models/` and checksum-valid, downloading it
/// only if missing or corrupt (lazy: called from the plugin's first `analyze()`, never at startup).
pub fn ensure_model(data_dir: &Path) -> Result<PathBuf, String> {
    let dir = data_dir.join("models");
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(MODEL_FILE);

    if path.is_file() {
        match sha256_file(&path) {
            Ok(sum) if sum == MODEL_SHA256 => return Ok(path),
            Ok(sum) => log::warn!("genre-embed: cached model checksum mismatch ({sum}), redownloading"),
            Err(e) => log::warn!("genre-embed: cannot checksum cached model ({e}), redownloading"),
        }
    }

    log::info!("genre-embed: downloading discogs-effnet model ({MODEL_URL})");
    let bytes = reqwest::blocking::get(MODEL_URL)
        .map_err(|e| format!("model download failed: {}", core::http::describe(&e)))?
        .error_for_status()
        .map_err(|e| format!("model download failed: {}", core::http::describe(&e)))?
        .bytes()
        .map_err(|e| format!("model download failed: {}", core::http::describe(&e)))?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sum: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if sum != MODEL_SHA256 {
        return Err(format!("downloaded model checksum {sum} does not match pinned {MODEL_SHA256}; rejecting"));
    }

    let tmp = dir.join(format!("{MODEL_FILE}.part"));
    std::fs::write(&tmp, &bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot install model: {e}"))?;
    log::info!("genre-embed: model downloaded and verified ({} bytes)", bytes.len());
    Ok(path)
}

/// Loads the ONNX model and selects the embedding output (see module doc). `discogs-effnet-bsdynamic-1`
/// has a dynamic batch dim, so no `with_input_fact` pinning is needed.
pub fn load(path: &Path) -> Result<Plan, String> {
    let mut model = tract_onnx::onnx().model_for_path(path).map_err(|e| format!("cannot parse model: {e}"))?;
    log::debug!(
        "genre-embed: model output outlets before selection: {:?}",
        model.output_outlets().map_err(|e| e.to_string())?
    );
    let outlet = model
        .find_outlet_label(OUTPUT_LABEL)
        .ok_or_else(|| format!("model has no output labeled \"{OUTPUT_LABEL}\""))?;
    model.select_output_outlets(&[outlet]).map_err(|e| format!("cannot select output: {e}"))?;
    let plan = model
        .into_optimized()
        .and_then(|m| m.into_runnable())
        .map_err(|e| format!("cannot build runnable model: {e}"))?;
    log::debug!("genre-embed: model loaded, output outlets: {:?}", plan.model().output_outlets());
    Ok(plan)
}

/// Runs `plan` over `frames` in `PATCH_FRAMES`-wide, `PATCH_HOP`-spaced patches, averaging the
/// resulting `[DIMS]` embeddings into one vector. `None` if `frames` is too short for one patch.
pub fn embed(plan: &Plan, frames: &[[f32; BANDS]]) -> Result<Option<Vec<f32>>, String> {
    if frames.len() < PATCH_FRAMES {
        return Ok(None);
    }
    let mut sum = vec![0.0_f32; DIMS];
    let mut count = 0usize;
    let mut start = 0;
    while start + PATCH_FRAMES <= frames.len() {
        let data: Vec<f32> = frames[start..start + PATCH_FRAMES].iter().flatten().copied().collect();
        let input = Tensor::from_shape(&[1, PATCH_FRAMES, BANDS], &data).map_err(|e| e.to_string())?;
        let out = plan.run(tvec!(input.into())).map_err(|e| e.to_string())?;
        let view = out[0].to_plain_array_view::<f32>().map_err(|e| e.to_string())?;
        for (s, v) in sum.iter_mut().zip(view.iter()) {
            *s += v;
        }
        count += 1;
        start += PATCH_HOP;
    }
    if count == 0 {
        return Ok(None);
    }
    for v in &mut sum {
        *v /= count as f32;
    }
    Ok(Some(sum))
}
