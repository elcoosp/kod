//! Common utilities for benchmarks.
//!
//! Provides benchmark environment setup and helper functions.

use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

#[allow(dead_code)]
/// Benchmark environment
pub struct BenchEnvironment {
    pub temp_dir: TempDir,
    pub working_dir: PathBuf,
    pub skills_dir: PathBuf,
}

impl BenchEnvironment {
    pub fn new() -> Self {
        let temp_dir = TempDir::new().unwrap();
        let working_dir = temp_dir.path().to_path_buf();
        let skills_dir = working_dir.join("skills");

        fs::create_dir_all(&skills_dir).unwrap();

        Self {
            temp_dir,
            working_dir,
            skills_dir,
        }
    }

    /// Add N skills to the environment
    pub fn add_skills(&self, count: usize) {
        for i in 0..count {
            let skill_content = format!(
                r#"---
name: skill-{}
description: Benchmark skill number {}
version: 1.0.0
category: benchmark
tags:
  - benchmark
  - test
capabilities:
  - benchmarking
triggers:
  - "benchmark {}"
---

## Instructions

Benchmark skill {} for performance testing.
"#,
                i, i, i, i
            );

            let file_name = format!("skill-{}.md", i);
            fs::write(self.skills_dir.join(file_name), skill_content).unwrap();
        }
    }
}

impl Default for BenchEnvironment {
    fn default() -> Self {
        Self::new()
    }
}
