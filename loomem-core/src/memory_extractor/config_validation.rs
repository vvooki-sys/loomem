use super::{ExtractionError, KnowledgeExtractionConfig};

pub(super) fn validated_chunk_size(
    config: &KnowledgeExtractionConfig,
) -> Result<usize, ExtractionError> {
    if config.max_facts_per_transcript == 0 {
        return Err(ExtractionError::InvalidConfig(
            "max_facts_per_transcript must be positive",
        ));
    }
    if config.max_tokens == 0 {
        return Err(ExtractionError::InvalidConfig(
            "max_tokens must be positive",
        ));
    }
    config
        .max_transcript_tokens
        .checked_mul(4)
        .filter(|size| *size > 0)
        .ok_or(ExtractionError::InvalidConfig(
            "max_transcript_tokens must be positive and fit the character budget",
        ))
}

pub(super) fn transcript_chunks(
    config: &KnowledgeExtractionConfig,
    text: &str,
) -> Result<Vec<String>, ExtractionError> {
    // Split into chunks if too long (rough estimate: 4 chars ≈ 1 token).
    let max_chars = validated_chunk_size(config)?;
    Ok(if text.len() > max_chars {
        super::split_with_overlap(text, max_chars, max_chars / 10)
    } else {
        vec![text.to_string()]
    })
}
