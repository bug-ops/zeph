---
aliases:
  - Injection Defense
  - IPI Protection
  - DeBERTa Injection Detection
  - AlignSentinel
  - PII NER Detection
tags:
  - sdd
  - spec
  - security
  - classifiers
  - contract
created: 2026-04-10
status: complete
related:
  - "[[010-security/spec]]"
  - "[[010-1-vault]]"
  - "[[010-3-authorization]]"
  - "[[010-4-audit]]"
  - "[[025-classifiers]]"
---

# Spec: Injection Defense & Content Isolation

Indirect Prompt Injection (IPI) defense, DeBERTa-backed detection, AlignSentinel confidence scoring, PII NER detection, content isolation.

## Overview

Zeph processes untrusted content from web scraping, MCP tool outputs, and user uploads. IPI attacks attempt to override agent behavior by embedding instructions in observed content. Zeph defends via multi-layer detection: regex, DeBERTa binary classifier, confidence scoring, and explicit user verification gates.

## Key Invariants

**Always:**
- All web-fetched content scanned for IPI patterns before returning to agent
- MCP tool outputs validated before reaching context/LLM
- Suspicious content flagged with confidence score; high scores require user verification
- PII (SSN, credit card, email) detected via NER and redacted or blocked

**Never:**
- Trust content from web pages, emails, or tool outputs without scanning
- Execute instructions found in observed content without explicit user approval
- Log secrets or PII in debug dumps
- Suppress IPI warnings when confidence is high

## IPI Detection Layers

```
Layer 1: Regex Patterns (Fast)
├─ "ignore this instruction"
├─ "prompt injection|jailbreak"
├─ SYSTEM: embedded in content
└─ Confidence: 0.95 (high)

Layer 2: DeBERTa Binary Classifier (Slow)
├─ Fine-tuned on IPI examples
├─ Outputs confidence [0.0–1.0]
└─ Run only if Layer 1 suspicious

Layer 3: AlignSentinel Confidence Scoring
├─ Combines regex + DeBERTa scores
├─ Context-aware adjustments
└─ > 0.8 = require user verification
```

Code:

```rust
pub struct IpiDetector {
    regex_patterns: Vec<(Regex, f32)>,     // (pattern, confidence)
    deberta: Arc<DeBERTaClassifier>,
    align_sentinel: Arc<AlignSentinel>,
}

impl IpiDetector {
    async fn scan_content(&self, text: &str) -> Result<ScanResult> {
        // Layer 1: Regex (fast)
        let mut max_regex_score = 0.0;
        for (pattern, confidence) in &self.regex_patterns {
            if pattern.is_match(text) {
                max_regex_score = max_regex_score.max(*confidence);
            }
        }
        
        // Short-circuit if regex very confident
        if max_regex_score > 0.9 {
            return Ok(ScanResult {
                is_injection: true,
                confidence: max_regex_score,
                detected_by: "regex",
                content_preview: truncate(text, 100),
            });
        }
        
        // Layer 2: DeBERTa (expensive; only if regex moderately suspicious)
        let deberta_score = if max_regex_score > 0.5 || text.len() > 500 {
            self.deberta.classify(text).await?
        } else {
            0.0
        };
        
        // Layer 3: AlignSentinel combines scores
        let final_score = self.align_sentinel.combine_scores(
            max_regex_score,
            deberta_score,
            text.len(),
        );
        
        Ok(ScanResult {
            is_injection: final_score > 0.75,
            confidence: final_score,
            detected_by: if deberta_score > max_regex_score {
                "deberta"
            } else {
                "regex"
            },
            content_preview: truncate(text, 100),
        })
    }
}
```

## PII Detection & Redaction

Named Entity Recognition for sensitive data:

```rust
pub struct PiiDetector {
    ner_model: Arc<NerClassifier>,  // Detects SSN, email, credit card, etc.
}

impl PiiDetector {
    async fn detect_pii(&self, text: &str) -> Result<Vec<PiiEntity>> {
        // NER inference
        let entities = self.ner_model.extract_entities(text).await?;
        
        Ok(entities
            .into_iter()
            .filter(|e| matches!(
                e.entity_type,
                EntityType::SSN
                    | EntityType::CreditCard
                    | EntityType::Email
                    | EntityType::PhoneNumber
            ))
            .collect())
    }
    
    async fn redact_pii(&self, text: &str) -> Result<String> {
        let entities = self.detect_pii(text).await?;
        
        let mut redacted = text.to_string();
        for entity in entities.iter().rev() {
            // Replace in reverse order to preserve offsets
            let replacement = match entity.entity_type {
                EntityType::SSN => "[REDACTED_SSN]",
                EntityType::CreditCard => "[REDACTED_CC]",
                EntityType::Email => "[REDACTED_EMAIL]",
                EntityType::PhoneNumber => "[REDACTED_PHONE]",
                _ => "[REDACTED]",
            };
            
            redacted.replace_range(entity.start..entity.end, replacement);
        }
        
        Ok(redacted)
    }
}
```

## Content Isolation Boundary

Web-fetched and tool-output content lives in isolated context:

```rust
pub struct IsolatedContent {
    // Content is NOT directly visible to LLM prompts
    original: String,
    scanned: bool,
    ipi_confidence: f32,
    pii_redacted: bool,
    source: ContentSource,
}

impl IsolatedContent {
    async fn release_to_agent(
        &self,
        detector: &IpiDetector,
        pii_detector: &PiiDetector,
    ) -> Result<String> {
        if !self.scanned {
            // Scan
            let scan = detector.scan_content(&self.original).await?;
            if scan.confidence > 0.75 {
                return Err(anyhow!(
                    "Content flagged as likely IPI (confidence: {:.0}%)",
                    scan.confidence * 100.0
                ));
            }
        }
        
        // Redact PII
        let redacted = pii_detector.redact_pii(&self.original).await?;
        
        Ok(redacted)
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ContentSource {
    WebFetch,
    McpToolOutput,
    UserUpload,
    Email,
}
```

## User Verification Gate

High-confidence IPI blocks agent until user approves:

```rust
async fn handle_suspicious_content(
    detector: &IpiDetector,
    content: &str,
) -> Result<String> {
    let scan = detector.scan_content(content).await?;
    
    if scan.confidence > 0.75 {
        // Block and ask user
        println!(
            "⚠️  Suspicious content detected (confidence: {:.0}%)",
            scan.confidence * 100.0
        );
        println!("Preview: {}", scan.content_preview);
        println!("Source: {:?}", scan.detected_by);
        println!("Should I use this content?");
        
        let user_approval = /* await user input */;
        if !user_approval {
            return Err(anyhow!("User rejected suspicious content"));
        }
    }
    
    Ok(content.to_string())
}
```

## Configuration

```toml
[security.injection_defense]
enabled = true
regex_patterns_enabled = true
deberta_enabled = true
confidence_threshold = 0.75      # require verification above this

# PII Detection
pii_detection_enabled = true
sensitive_types = ["ssn", "credit_card", "email", "phone"]
redact_mode = "mask"             # or "block"
```

## Integration Points

- [[008-mcp]] — MCP tool outputs scanned before reaching agent
- [[025-classifiers]] — DeBERTa inference infrastructure
- [[010-4-audit]] — IPI detection logged for compliance
- WebFetch tool — Content scanned before returning

## See Also

- [[010-security/spec]] — Parent
- [[010-1-vault]] — Prevent secret leakage from redaction
- [[025-classifiers]] — DeBERTa and NER models
- [[010-4-audit]] — Audit log of IPI detections
