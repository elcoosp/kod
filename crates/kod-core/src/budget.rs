//! Prompt budget (D6-M5).
//!
//! The per-section caps (repomap 16k chars, skills N × 1.5k, memory
//! `context_window × 4` chars, history 32k chars) each look reasonable
//! on their own, and each is applied by a different piece of code. On
//! a small model they add up to more than the model's window: a fresh
//! session on an 8k-token model could ship a 12k-token prompt and the
//! server rejects it (or, worse, silently truncates).
//!
//! `PromptBudget` is the single allocation point. It is given the
//! model's context window in tokens, a reserve for the completion
//! (the endpoint's `max_tokens`), and the size of the user's request
//! (which is not truncatable), and returns per-section char budgets
//! the router honours.
//!
//! # Ordering
//!
//! Priority, highest first:
//!
//! 1. **request** — never truncated. A request that does not fit is
//!    an error the caller surfaces; silently truncating what the user
//!    asked would be worse than a clear "your prompt is too long".
//! 2. **history** — the recent transcript. Half of the remaining
//!    budget, because a session that forgets its last two turns is
//!    useless.
//! 3. **skills** — the matched skill instructions.
//! 4. **memory** — long-term entries.
//! 5. **repomap** — nice to have; the model can still read files.
//!
//! Each of history / skills / memory / repomap takes a fixed share of
//! what remains after the request: 50% / 20% / 20% / 10%. The shares
//! are character-based, matching how the caps are already expressed in
//! the codebase (the roadmap's convention is 1 token ≈ 4 chars).

/// The character-count allocation for one prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    /// Maximum chars for the user's request. Always the request's real
    /// length when it fits; smaller than the request when it does not
    /// (the caller must reject).
    pub request: usize,
    pub history: usize,
    pub skills: usize,
    pub memory: usize,
    pub repomap: usize,
}

impl Allocation {
    /// The total chars the four truncatable sections get. The request
    /// is not counted here; it is subtracted up front.
    pub fn truncatable_total(&self) -> usize {
        self.history + self.skills + self.memory + self.repomap
    }
}

/// A prompt budget derived from the model's window.
#[derive(Debug, Clone, Copy)]
pub struct PromptBudget {
    /// Total chars available for the whole prompt (everything the
    /// model sees).
    pub total_chars: usize,
}

impl PromptBudget {
    /// Chars-per-token used throughout the codebase. One place to
    /// change if a future model tokenizer disagrees.
    pub const CHARS_PER_TOKEN: usize = 4;

    /// Build from the endpoint's token counts.
    ///
    /// `context_window` is the model's window in tokens; `reserve_out`
    /// is what we keep free for the completion. A reserve of 0 is
    /// treated as "reserve 20% of the window", which is the safe
    /// default if a caller forgets.
    pub fn from_tokens(context_window: usize, reserve_out: usize) -> Self {
        let reserve = if reserve_out == 0 {
            context_window / 5
        } else {
            reserve_out
        };
        let usable_tokens = context_window.saturating_sub(reserve);
        Self {
            total_chars: usable_tokens.saturating_mul(Self::CHARS_PER_TOKEN),
        }
    }

    /// Allocate section budgets for a request of `request_chars`.
    ///
    /// `Err` when the request alone does not fit — the caller should
    /// return a clear error rather than silently ship a truncated
    /// prompt. The error string names both numbers so a user can see
    /// how much they need to trim or how big a window to configure.
    pub fn allocate(&self, request_chars: usize) -> Result<Allocation, BudgetError> {
        if request_chars > self.total_chars {
            return Err(BudgetError {
                request_chars,
                total_chars: self.total_chars,
            });
        }
        let remaining = self.total_chars - request_chars;
        // Shares sum to 100%: history 50, skills 20, memory 20,
        // repomap 10. Integer arithmetic, no float drift.
        let history = remaining * 50 / 100;
        let skills = remaining * 20 / 100;
        let memory = remaining * 20 / 100;
        let repomap = remaining.saturating_sub(history + skills + memory);
        Ok(Allocation {
            request: request_chars,
            history,
            skills,
            memory,
            repomap,
        })
    }
}

/// The request does not fit the window at all.
#[derive(Debug, Clone, Copy)]
pub struct BudgetError {
    pub request_chars: usize,
    pub total_chars: usize,
}

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let est_tokens = self.request_chars / PromptBudget::CHARS_PER_TOKEN;
        let window_tokens = self.total_chars / PromptBudget::CHARS_PER_TOKEN;
        write!(
            f,
            "the request is too long for this model: {} chars (~{} tokens) \
             against a usable prompt budget of {} chars (~{} tokens). \
             Shorten the request, or set a larger context_window on the \
             endpoint.",
            self.request_chars, est_tokens, self.total_chars, window_tokens,
        )
    }
}

impl std::error::Error for BudgetError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_sum_to_the_whole_remainder() {
        let b = PromptBudget::from_tokens(8192, 2048);
        let a = b.allocate(0).unwrap();
        assert_eq!(
            a.truncatable_total(),
            b.total_chars,
            "the four shares must consume the whole remainder"
        );
    }

    #[test]
    fn request_that_fits_is_preserved() {
        let b = PromptBudget::from_tokens(8192, 2048);
        let a = b.allocate(500).unwrap();
        assert_eq!(a.request, 500);
    }

    #[test]
    fn oversized_request_errors() {
        let b = PromptBudget::from_tokens(2048, 512);
        // 2048 - 512 = 1536 tokens * 4 = 6144 chars usable. A 10k
        // request is over.
        let err = b.allocate(10_000).unwrap_err();
        assert_eq!(err.request_chars, 10_000);
        // The message names both sizes so the user can act.
        let msg = err.to_string();
        assert!(msg.contains("too long"), "{msg}");
        assert!(msg.contains("context_window"), "{msg}");
    }

    #[test]
    fn zero_reserve_defaults_to_twenty_percent() {
        let b = PromptBudget::from_tokens(1000, 0);
        // 1000 * 0.8 = 800 tokens * 4 chars = 3200 chars.
        assert_eq!(b.total_chars, 3200);
    }

    #[test]
    fn history_gets_the_largest_share() {
        let b = PromptBudget::from_tokens(10_000, 2_000);
        let a = b.allocate(0).unwrap();
        assert!(a.history > a.skills, "history should outrank skills");
        assert!(a.history > a.memory, "history should outrank memory");
        assert!(a.skills > a.repomap, "skills should outrank repomap");
        assert!(a.memory > a.repomap, "memory should outrank repomap");
    }

    #[test]
    fn larger_window_yields_a_larger_budget() {
        let small = PromptBudget::from_tokens(8192, 2048).allocate(100).unwrap();
        let large = PromptBudget::from_tokens(131_072, 4096).allocate(100).unwrap();
        assert!(large.truncatable_total() > small.truncatable_total() * 10);
    }
}
