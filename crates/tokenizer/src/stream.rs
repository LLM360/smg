// src/tokenizer/stream.rs

use std::sync::Arc;

use anyhow::Result;

use crate::traits::{self, TokenIdType};

const INITIAL_INCREMENTAL_DETOKENIZATION_OFFSET: usize = 5;

/// DecodeStream will keep the state necessary to produce individual chunks of
/// strings given an input stream of token_ids
pub struct DecodeStream {
    /// The tokenizer used to decode token_ids
    tokenizer: Arc<dyn traits::Tokenizer>,

    skip_special_tokens: bool,

    /// A temporary buffer of the necessary token_ids needed
    /// to produce valid string chunks
    all_token_ids: Vec<TokenIdType>,

    prefix_offset: usize,
    read_offset: usize,
}

impl DecodeStream {
    pub fn new(
        tokenizer: Arc<dyn traits::Tokenizer>,
        prompt_token_ids: &[TokenIdType],
        skip_special_tokens: bool,
    ) -> Self {
        let num_input_tokens = prompt_token_ids.len();
        let prompt_token_ids = prompt_token_ids.to_vec();
        Self {
            tokenizer,
            skip_special_tokens,
            all_token_ids: prompt_token_ids,
            prefix_offset: num_input_tokens
                .saturating_sub(INITIAL_INCREMENTAL_DETOKENIZATION_OFFSET),
            read_offset: num_input_tokens,
        }
    }

    /// Step appends a token_id to the internal state and tries to produce a text chunk.
    /// Returning `None` means the given id is not enough to produce a chunk.
    #[inline]
    pub fn step(&mut self, id: TokenIdType) -> Result<Option<String>> {
        self.all_token_ids.push(id);

        let prefix_text = self.tokenizer.decode(
            &self.all_token_ids[self.prefix_offset..self.read_offset],
            self.skip_special_tokens,
        )?;

        let new_text = self.tokenizer.decode(
            &self.all_token_ids[self.prefix_offset..],
            self.skip_special_tokens,
        )?;

        if new_text.len() > prefix_text.len() && !new_text.ends_with("�") {
            // Find the nearest char boundary at or before prefix_text.len()
            // to avoid panicking on multi-byte UTF-8 sequences
            let mut split_at = prefix_text.len();
            while !new_text.is_char_boundary(split_at) && split_at > 0 {
                split_at -= 1;
            }

            let new_text = new_text[split_at..].to_string();

            self.prefix_offset = self.read_offset;
            self.read_offset = self.all_token_ids.len();

            Ok(Some(new_text))
        } else {
            Ok(None)
        }
    }

    /// Process multiple tokens at once
    pub fn step_batch(&mut self, token_ids: &[u32]) -> Result<Vec<String>> {
        // Pre-allocate with capacity - most tokens produce output
        let mut chunks = Vec::with_capacity(token_ids.len());

        for &token_id in token_ids {
            if let Some(text) = self.step(token_id)? {
                chunks.push(text);
            }
        }

        Ok(chunks)
    }

    /// Force flush any remaining text
    pub fn flush(&mut self) -> Result<Option<String>> {
        if self.read_offset < self.all_token_ids.len() {
            let remaining = self.tokenizer.decode(
                &self.all_token_ids[self.read_offset..],
                self.skip_special_tokens,
            )?;

            self.read_offset = self.all_token_ids.len();

            if !remaining.is_empty() {
                return Ok(Some(remaining));
            }
        }

        Ok(None)
    }

    /// Get all tokens processed so far
    pub fn tokens(&self) -> &[u32] {
        &self.all_token_ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tiktoken::{TiktokenModel, TiktokenTokenizer};

    /// Emoji plus CJK sample: several multi-byte sequences across scripts.
    const MULTIBYTE_TEXT: &str = "Hello 世界 😀 こんにちは 👍🏽!";

    fn cl100k_tokenizer() -> Arc<dyn traits::Tokenizer> {
        Arc::new(TiktokenTokenizer::new(TiktokenModel::Cl100kBase).unwrap())
    }

    fn encode_ids(tokenizer: &Arc<dyn traits::Tokenizer>, text: &str) -> Vec<TokenIdType> {
        tokenizer
            .encode(text, false)
            .expect("encode must succeed")
            .token_ids()
            .to_vec()
    }

    /// Crate-boundary lock (LLM360/comet#3719): one token at a time, the
    /// stream round-trips emoji+CJK exactly, and no emitted chunk ever
    /// contains U+FFFD.
    #[test]
    fn test_multibyte_round_trip_single_token_steps() {
        let tokenizer = cl100k_tokenizer();
        let ids = encode_ids(&tokenizer, MULTIBYTE_TEXT);
        assert!(ids.len() > 1);

        let mut stream = DecodeStream::new(Arc::clone(&tokenizer), &[], true);
        let mut emitted = String::new();
        for &id in &ids {
            if let Some(chunk) = stream.step(id).expect("decode step") {
                assert!(
                    !chunk.contains('\u{FFFD}'),
                    "emitted chunk must not contain U+FFFD: {chunk:?}"
                );
                emitted.push_str(&chunk);
            }
        }
        assert_eq!(stream.flush().expect("flush"), None);
        assert_eq!(emitted, MULTIBYTE_TEXT);
    }

    /// Crate-boundary lock (LLM360/comet#3719): the same round trip through
    /// two-token steps, the shape of the small /generate backend deltas.
    #[test]
    fn test_multibyte_round_trip_two_token_steps() {
        let tokenizer = cl100k_tokenizer();
        let ids = encode_ids(&tokenizer, MULTIBYTE_TEXT);

        let mut stream = DecodeStream::new(Arc::clone(&tokenizer), &[], true);
        let mut emitted = String::new();
        for window in ids.chunks(2) {
            for chunk in stream.step_batch(window).expect("decode step batch") {
                assert!(
                    !chunk.contains('\u{FFFD}'),
                    "emitted chunk must not contain U+FFFD: {chunk:?}"
                );
                emitted.push_str(&chunk);
            }
        }
        assert_eq!(stream.flush().expect("flush"), None);
        assert_eq!(emitted, MULTIBYTE_TEXT);
    }

    /// Crate-boundary lock (LLM360/comet#3719): a truncated final sequence
    /// (a simulated `max_new_tokens` cut inside a multi-byte character)
    /// leaves the already emitted text untouched; the residual tail — the
    /// deliberate lossy part — comes only from `flush()`.
    #[test]
    fn test_truncated_tail_leaves_emissions_and_flushes_lossy_tail() {
        let tokenizer = cl100k_tokenizer();
        let ids = encode_ids(&tokenizer, "分割😀");
        let cut = (1..ids.len())
            .rev()
            .find(|&cut| {
                tokenizer
                    .decode(&ids[..cut], true)
                    .expect("decode")
                    .contains('\u{FFFD}')
            })
            .expect("some truncation of 分割😀 must end inside a character");

        let mut stream = DecodeStream::new(Arc::clone(&tokenizer), &[], true);
        let mut emitted = String::new();
        for chunk in stream.step_batch(&ids[..cut]).expect("decode step batch") {
            assert!(
                !chunk.contains('\u{FFFD}'),
                "emitted chunk must not contain U+FFFD: {chunk:?}"
            );
            emitted.push_str(&chunk);
        }
        assert!(
            "分割😀".starts_with(&emitted),
            "emitted text stays a prefix of the full text: {emitted:?}"
        );

        let tail = stream.flush().expect("flush");
        let tail = tail.expect("flush emits the residual tail");
        assert!(
            tail.contains('\u{FFFD}'),
            "the residual tail is deliberately lossy: {tail:?}"
        );
    }
}
