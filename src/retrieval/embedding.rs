use std::{fs, path::Path, sync::Arc};

use anyhow::{Context, Result, bail, ensure};
use half::f16;
use safetensors::{Dtype, SafeTensors};
use tokenizers::Tokenizer;

pub const MODEL_ID: &str = "minishlab/potion-code-16M-v2";
pub const MODEL_REVISION: &str = "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b";
pub const EMBEDDING_DIMENSION: usize = 256;
const MAX_TOKENS: usize = 1024;
const TENSOR_NAME: &str = "embeddings";

/// A shareable, immutable Model2Vec encoder for the repository's fixed model.
#[derive(Clone)]
pub struct Model2VecEmbedding {
    inner: Arc<Inner>,
}

struct Inner {
    tokenizer: Tokenizer,
    embeddings: Vec<f32>,
    vocab_size: usize,
    unk_id: Option<u32>,
}

impl Model2VecEmbedding {
    /// Loads `tokenizer.json` and `model.safetensors` from `model_dir`.
    pub fn load(model_dir: impl AsRef<Path>) -> Result<Self> {
        let model_dir = model_dir.as_ref();
        let tokenizer_path = model_dir.join("tokenizer.json");
        let weights_path = model_dir.join("model.safetensors");
        ensure!(
            tokenizer_path.is_file(),
            "missing tokenizer file: {}",
            tokenizer_path.display()
        );
        ensure!(
            weights_path.is_file(),
            "missing model weights file: {}",
            weights_path.display()
        );

        let tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .with_context(|| format!("failed to load tokenizer: {}", tokenizer_path.display()))?;
        let unk_id = tokenizer_unknown_id(&tokenizer_path, &tokenizer)?;
        let bytes = fs::read(&weights_path)
            .with_context(|| format!("failed to read model weights: {}", weights_path.display()))?;
        let tensors = SafeTensors::deserialize(&bytes)
            .with_context(|| format!("invalid safetensors file: {}", weights_path.display()))?;
        let tensor = tensors.tensor(TENSOR_NAME).with_context(|| {
            format!(
                "required tensor {TENSOR_NAME:?} is missing from {}",
                weights_path.display()
            )
        })?;
        ensure!(
            tensor.dtype() == Dtype::F16,
            "tensor {TENSOR_NAME:?} has type {:?}; expected F16",
            tensor.dtype()
        );
        let shape = tensor.shape();
        ensure!(
            shape.len() == 2 && shape[1] == EMBEDDING_DIMENSION,
            "tensor {TENSOR_NAME:?} has shape {shape:?}; expected vocab×{EMBEDDING_DIMENSION}"
        );
        let vocab_size = shape[0];
        ensure!(
            tokenizer.get_vocab_size(true) <= vocab_size,
            "tokenizer vocabulary size {} exceeds embedding vocabulary size {vocab_size}",
            tokenizer.get_vocab_size(true)
        );
        let element_count = vocab_size
            .checked_mul(EMBEDDING_DIMENSION)
            .context("embedding tensor shape overflows addressable memory")?;
        let expected_bytes = element_count
            .checked_mul(2)
            .context("embedding tensor byte length overflows addressable memory")?;
        ensure!(
            tensor.data().len() == expected_bytes,
            "tensor {TENSOR_NAME:?} byte length {} does not match shape {shape:?} and F16 type",
            tensor.data().len()
        );
        let embeddings = tensor
            .data()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| f16::from_bits(u16::from_le_bytes([bytes[0], bytes[1]])).to_f32())
            .collect();

        Ok(Self {
            inner: Arc::new(Inner {
                tokenizer,
                embeddings,
                vocab_size,
                unk_id,
            }),
        })
    }

    pub fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let encoding = self
            .inner
            .tokenizer
            .encode(text, false)
            .map_err(|error| anyhow::anyhow!("tokenizer failed to encode input: {error}"))?;
        pool(
            encoding.get_ids(),
            self.inner.unk_id,
            &self.inner.embeddings,
            self.inner.vocab_size,
            MAX_TOKENS,
        )
    }

    pub fn embed_batch<S: AsRef<str>>(&self, texts: &[S]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|text| self.embed(text.as_ref())).collect()
    }
}

fn tokenizer_unknown_id(path: &Path, tokenizer: &Tokenizer) -> Result<Option<u32>> {
    let value: serde_json::Value = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("failed to read tokenizer: {}", path.display()))?,
    )
    .with_context(|| format!("invalid tokenizer JSON: {}", path.display()))?;
    let Some(unk_token) = value
        .get("model")
        .and_then(|model| model.get("unk_token"))
        .and_then(|token| token.as_str())
    else {
        return Ok(None);
    };
    tokenizer.token_to_id(unk_token).map(Some).with_context(|| {
        format!("tokenizer declares UNK token {unk_token:?}, but it is absent from the vocabulary")
    })
}

fn pool(
    token_ids: &[u32],
    unk_id: Option<u32>,
    embeddings: &[f32],
    vocab_size: usize,
    max_tokens: usize,
) -> Result<Vec<f32>> {
    let mut output = vec![0.0_f32; EMBEDDING_DIMENSION];
    let mut count = 0_usize;
    for &id in token_ids.iter().take(max_tokens) {
        if Some(id) == unk_id {
            continue;
        }
        let row = id as usize;
        if row >= vocab_size {
            bail!("token id {id} is out of bounds for embedding vocabulary of size {vocab_size}");
        }
        let start = row * EMBEDDING_DIMENSION;
        for (sum, value) in output
            .iter_mut()
            .zip(&embeddings[start..start + EMBEDDING_DIMENSION])
        {
            *sum += value;
        }
        count += 1;
    }
    if count == 0 {
        return Ok(output);
    }
    let divisor = count as f32;
    for value in &mut output {
        *value /= divisor;
    }
    let norm = output.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut output {
            *value /= norm;
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(values: &[f32]) -> Vec<f32> {
        values
            .iter()
            .flat_map(|&value| std::iter::repeat_n(value, EMBEDDING_DIMENSION))
            .collect()
    }

    #[test]
    fn pools_in_token_order_and_normalizes() {
        let mut embeddings = vec![0.0; 3 * EMBEDDING_DIMENSION];
        embeddings[0] = 1.0e10;
        embeddings[EMBEDDING_DIMENSION] = -1.0e10;
        embeddings[2 * EMBEDDING_DIMENSION] = 1.0;
        let forward = pool(&[0, 1, 2], None, &embeddings, 3, MAX_TOKENS).unwrap();
        let reverse = pool(&[0, 2, 1], None, &embeddings, 3, MAX_TOKENS).unwrap();
        assert_eq!(forward[0], 1.0);
        assert_eq!(reverse[0], 0.0);

        let mut embeddings = vec![0.0; 2 * EMBEDDING_DIMENSION];
        embeddings[0] = 3.0;
        embeddings[EMBEDDING_DIMENSION + 1] = 4.0;
        let result = pool(&[0, 1], None, &embeddings, 2, MAX_TOKENS).unwrap();
        assert!((result[0] - 0.6).abs() < 1e-6);
        assert!((result[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn empty_and_all_unknown_are_zero() {
        let embeddings = rows(&[1.0]);
        assert_eq!(
            pool(&[], None, &embeddings, 1, MAX_TOKENS).unwrap(),
            vec![0.0; 256]
        );
        assert_eq!(
            pool(&[0, 0], Some(0), &embeddings, 1, MAX_TOKENS).unwrap(),
            vec![0.0; 256]
        );
    }

    #[test]
    fn truncates_before_pooling() {
        let embeddings = rows(&[1.0, -1.0]);
        let mut ids = vec![0; MAX_TOKENS];
        ids.push(1);
        let result = pool(&ids, None, &embeddings, 2, MAX_TOKENS).unwrap();
        assert!(result.iter().all(|value| *value > 0.0));
    }
}
