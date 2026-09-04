# Chunk 5: Tool System Implementation

## Task 24: Tool Trait and Registry

**Files:**
- Modify: `crates/kod-tools/Cargo.toml`
- Create: `crates/kod-tools/src/lib.rs`
- Create: `crates/kod-tools/src/registry.rs`
- Create: `crates/kod-tools/src/context.rs`
- Test: `crates/kod-tools/tests/registry.rs`

- [ ] **Step 1: Update kod-tools Cargo.toml**

```toml
[package]
name = "kod-tools"
version.workspace = true
edition.workspace = true
license.workspace = true
authors.workspace = true

[dependencies]
async-trait = "0.1"
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
walkdir = { workspace = true }
globset = { workspace = true }
fs4 = { workspace = true }
kod-types = { path = "../kod-types" }
kod-error = { path = "../kod-error" }

[dev-dependencies]
rstest = { workspace = true }
tempfile = "3.8"
```

- [ ] **Step 2: Write failing test for tool registry**

Create `crates/kod-tools/tests/registry.rs`:

```rust
use kod_tools::registry::ToolRegistry;
use kod_tools::{Tool, ToolContext, ToolResult};
use kod_types::{ToolCategory, ToolId, ToolPermissions};
use async_trait::async_trait;
use serde_json::json;
use std::path::PathBuf;

// Test tool implementation
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn definition(&self) -> kod_types::ToolDefinition {
        kod_types::ToolDefinition {
            id: ToolId::new(),
            name: "echo".to_string(),
            description: "Echo back the input".to_string(),
            category: ToolCategory::System,
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": "Message to echo"
                    }
                },
                "required": ["message"]
            }),
            permissions: ToolPermissions {
                read_files: false,
                write_files: false,
                execute_commands: false,
                network_access: false,
                git_operations: false,
                allowed_paths: Vec::new(),
                forbidden_paths: Vec::new(),
            },
        }
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult, kod_error::KodError> {
        let message = params["message"].as_str().unwrap_or("");
        Ok(ToolResult::Success(json!({
            "echo": message,
            "working_dir": context.working_dir.display().to_string(),
        })))
    }
}

#[tokio::test]
async fn test_register_and_get_tool() {
    let mut registry = ToolRegistry::new();
    
    let tool = EchoTool;
    let tool_name = tool.definition().name.clone();
    
    registry.register(Box::new(tool));
    
    let retrieved = registry.get(&tool_name);
    assert!(retrieved.is_some());
    
    let not_found = registry.get("nonexistent");
    assert!(not_found.is_none());
}

#[tokio::test]
async fn test_list_tools_by_category() {
    let mut registry = ToolRegistry::new();
    
    registry.register(Box::new(EchoTool));
    
    let system_tools = registry.list_by_category(ToolCategory::System);
    assert_eq!(system_tools.len(), 1);
    
    let file_tools = registry.list_by_category(ToolCategory::FileSystem);
    assert_eq!(file_tools.len(), 0);
}

#[tokio::test]
async fn test_list_all_tools() {
    let mut registry = ToolRegistry::new();
    
    registry.register(Box::new(EchoTool));
    
    let all = registry.list_all();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["name"], "echo");
}

#[tokio::test]
async fn test_remove_tool() {
    let mut registry = ToolRegistry::new();
    
    let tool = EchoTool;
    let tool_name = tool.definition().name.clone();
    
    registry.register(Box::new(tool));
    assert!(registry.get(&tool_name).is_some());
    
    registry.remove(&tool_name);
    assert!(registry.get(&tool_name).is_none());
}

#[tokio::test]
async fn test_tool_count() {
    let mut registry = ToolRegistry::new();
    assert_eq!(registry.count(), 0);
    
    registry.register(Box::new(EchoTool));
    assert_eq!(registry.count(), 1);
}

#[tokio::test]
async fn test_get_tool_definitions_for_llm() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool));
    
    let definitions = registry.get_definitions_for_llm();
    assert_eq!(definitions.len(), 1);
    
    // Should be in OpenAI function calling format
    assert_eq!(definitions[0]["type"], "function");
    assert_eq!(definitions[0]["function"]["name"], "echo");
    assert!(definitions[0]["function"]["parameters"].is_object());
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test -p kod-tools --test registry
```

Expected: FAIL - registry module not implemented

- [ ] **Step 4: Implement tool trait and registry**

Create `crates/kod-tools/src/lib.rs`:

```rust
//! Tool system for agent-environment interaction.
//!
//! Provides the Tool trait, registry, and execution context
//! for tools that can be called by the AI agent.

pub mod registry;
pub mod context;
pub mod tools;
pub mod executor;

pub use registry::ToolRegistry;
pub use context::{ToolContext, ToolPermissions};
pub use executor::ToolExecutor;

// Re-export tool trait for convenience
pub use kod_types::{ToolDefinition, ToolResult};

/// Trait that all tools must implement
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    /// Get the tool definition (name, description, schema, permissions)
    fn definition(&self) -> ToolDefinition;
    
    /// Execute the tool with the given parameters
    async fn execute(
        &self,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult, kod_error::KodError>;
}
```

Create `crates/kod-tools/src/registry.rs`:

```rust
//! Tool registry - manages available tools and their definitions.

use crate::Tool;
use kod_types::ToolCategory;
use std::collections::HashMap;
use tokio::sync::RwLock;

/// Registry that manages all available tools
#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: RwLock<HashMap<String, Box<dyn Tool>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
        }
    }

    /// Register a new tool
    pub async fn register(&self, tool: Box<dyn Tool>) {
        let name = tool.definition().name.clone();
        self.tools.write().await.insert(name, tool);
    }

    /// Remove a tool by name
    pub async fn remove(&self, name: &str) {
        self.tools.write().await.remove(name);
    }

    /// Get a tool by name
    pub async fn get(&self, name: &str) -> Option<&dyn Tool> {
        let tools = self.tools.read().await;
        tools.get(name).map(|t| t.as_ref())
    }

    /// Check if a tool exists
    pub async fn has(&self, name: &str) -> bool {
        self.tools.read().await.contains_key(name)
    }

    /// List all tools
    pub async fn list_all(&self) -> Vec<String> {
        self.tools.read().await.keys().cloned().collect()
    }

    /// List tools by category
    pub async fn list_by_category(&self, category: ToolCategory) -> Vec<String> {
        let tools = self.tools.read().await;
        tools.values()
            .filter(|t| t.definition().category == category)
            .map(|t| t.definition().name.clone())
            .collect()
    }

    /// Get tool count
    pub async fn count(&self) -> usize {
        self.tools.read().await.len()
    }

    /// Get tool definitions formatted for LLM function calling
    pub async fn get_definitions_for_llm(&self) -> Vec<serde_json::Value> {
        let tools = self.tools.read().await;
        
        tools.values()
            .map(|tool| {
                let def = tool.definition();
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": def.name,
                        "description": def.description,
                        "parameters": def.parameters_schema,
                    }
                })
            })
            .collect()
    }

    /// Get tool permissions by name
    pub async fn get_permissions(&self, name: &str) -> Option<kod_types::ToolPermissions> {
        let tools = self.tools.read().await;
        tools.get(name).map(|t| t.definition().permissions)
    }
}

// Synchronous version for non-async contexts
impl ToolRegistry {
    /// Register a tool (sync version for initialization)
    pub fn register_sync(&mut self, tool: Box<dyn Tool>) {
        let name = tool.definition().name.clone();
        // Convert to async in a blocking manner for init
        futures::executor::block_on(async {
            self.register(tool).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Tool, ToolContext};
    use kod_types::{ToolCategory, ToolId, ToolPermissions, ToolResult};
    use async_trait::async_trait;

    struct TestTool;

    #[async_trait]
    impl Tool for TestTool {
        fn definition(&self) -> kod_types::ToolDefinition {
            kod_types::ToolDefinition {
                id: ToolId::new(),
                name: "test".to_string(),
                description: "Test tool".to_string(),
                category: ToolCategory::System,
                parameters_schema: serde_json::json!({}),
                permissions: ToolPermissions::default(),
            }
        }

        async fn execute(
            &self,
            _params: &serde_json::Value,
            _context: &ToolContext,
        ) -> Result<ToolResult, kod_error::KodError> {
            Ok(ToolResult::Success(serde_json::json!({"test": true})))
        }
    }

    #[tokio::test]
    async fn test_registry_operations() {
        let registry = ToolRegistry::new();
        
        registry.register(Box::new(TestTool)).await;
        
        assert!(registry.has("test").await);
        assert_eq!(registry.count().await, 1);
        
        let tool = registry.get("test").await;
        assert!(tool.is_some());
        
        registry.remove("test").await;
        assert!(!registry.has("test").await);
    }
}
```

- [ ] **Step 5: Implement tool execution context**

Create `crates/kod-tools/src/context.rs`:

```rust
//! Tool execution context and permissions.

use kod_error::{KodError, Result};
use kod_types::ToolPermissions;
use std::path::{Path, PathBuf};

/// Context for tool execution
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Working directory for relative paths
    pub working_dir: PathBuf,
    
    /// Permissions for this execution
    pub permissions: ToolPermissions,
    
    /// Timeout for execution (in seconds)
    pub timeout_secs: u64,
}

impl ToolContext {
    /// Create a new context with default permissions
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            permissions: ToolPermissions::default(),
            timeout_secs: 30,
        }
    }

    /// Set permissions
    pub fn with_permissions(mut self, permissions: ToolPermissions) -> Self {
        self.permissions = permissions;
        self
    }

    /// Set timeout
    pub fn with_timeout(mut self, timeout_secs: u64) -> Self {
        self.timeout_secs = timeout_secs;
        self
    }

    /// Resolve a path relative to working directory
    pub fn resolve_path(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.working_dir.join(path)
        }
    }

    /// Check if path is allowed by permissions
    pub fn is_path_allowed(&self, path: &Path) -> Result<bool> {
        // Check forbidden paths first
        for forbidden in &self.permissions.forbidden_paths {
            if self.matches_pattern(path, forbidden) {
                return Ok(false);
            }
        }
        
        // If allowed_paths is empty, allow all (except forbidden)
        if self.permissions.allowed_paths.is_empty() {
            return Ok(true);
        }
        
        // Check allowed paths
        for allowed in &self.permissions.allowed_paths {
            if self.matches_pattern(path, allowed) {
                return Ok(true);
            }
        }
        
        Ok(false)
    }

    /// Check if path can be read
    pub fn can_read(&self, path: &Path) -> Result<()> {
        if !self.permissions.read_files {
            return Err(KodError::PermissionDenied {
                action: "read".to_string(),
                reason: "File reading not permitted".to_string(),
            });
        }
        
        if !self.is_path_allowed(path)? {
            return Err(KodError::PermissionDenied {
                action: "read".to_string(),
                reason: format!("Path not allowed: {}", path.display()),
            });
        }
        
        Ok(())
    }

    /// Check if path can be written
    pub fn can_write(&self, path: &Path) -> Result<()> {
        if !self.permissions.write_files {
            return Err(KodError::PermissionDenied {
                action: "write".to_string(),
                reason: "File writing not permitted".to_string(),
            });
        }
        
        if !self.is_path_allowed(path)? {
            return Err(KodError::PermissionDenied {
                action: "write".to_string(),
                reason: format!("Path not allowed: {}", path.display()),
            });
        }
        
        Ok(())
    }

    /// Check if command can be executed
    pub fn can_execute_command(&self, command: &str) -> Result<()> {
        if !self.permissions.execute_commands {
            return Err(KodError::PermissionDenied {
                action: "execute".to_string(),
                reason: "Command execution not permitted".to_string(),
            });
        }
        
        // Check for dangerous commands
        let dangerous_patterns = [
            "rm -rf /",
            "mkfs",
            "dd if=",
            "format",
            "shutdown",
            "reboot",
            "sudo",
        ];
        
        let command_lower = command.to_lowercase();
        for pattern in &dangerous_patterns {
            if command_lower.contains(pattern) {
                return Err(KodError::SandboxViolation(format!(
                    "Dangerous command detected: {}",
                    pattern
                )));
            }
        }
        
        Ok(())
    }

    /// Check if git operations are allowed
    pub fn can_git(&self) -> Result<()> {
        if !self.permissions.git_operations {
            return Err(KodError::PermissionDenied {
                action: "git".to_string(),
                reason: "Git operations not permitted".to_string(),
            });
        }
        
        Ok(())
    }

    /// Check if network access is allowed
    pub fn can_network(&self) -> Result<()> {
        if !self.permissions.network_access {
            return Err(KodError::PermissionDenied {
                action: "network".to_string(),
                reason: "Network access not permitted".to_string(),
            });
        }
        
        Ok(())
    }

    /// Check if path matches a glob pattern
    fn matches_pattern(&self, path: &Path, pattern: &str) -> bool {
        use globset::GlobBuilder;
        
        match GlobBuilder::new(pattern).build() {
            Ok(glob) => glob.compile_matcher().is_match(path),
            Err(_) => path.to_str().map(|p| p.contains(pattern)).unwrap_or(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_path_resolution() {
        let temp_dir = TempDir::new().unwrap();
        let context = ToolContext::new(temp_dir.path());
        
        // Relative path
        let resolved = context.resolve_path("test.rs");
        assert_eq!(resolved, temp_dir.path().join("test.rs"));
        
        // Absolute path
        let abs_path = "/usr/local/bin";
        let resolved = context.resolve_path(abs_path);
        assert_eq!(resolved, PathBuf::from(abs_path));
    }

    #[test]
    fn test_permission_checks() {
        let temp_dir = TempDir::new().unwrap();
        let mut context = ToolContext::new(temp_dir.path());
        
        // Default: no permissions
        assert!(context.can_read(temp_dir.path()).is_err());
        assert!(context.can_write(temp_dir.path()).is_err());
        assert!(context.can_execute_command("ls").is_err());
        assert!(context.can_git().is_err());
        assert!(context.can_network().is_err());
        
        // Grant permissions
        context.permissions.read_files = true;
        context.permissions.write_files = true;
        
        assert!(context.can_read(temp_dir.path()).is_ok());
        assert!(context.can_write(temp_dir.path()).is_ok());
    }

    #[test]
    fn test_path_restrictions() {
        let temp_dir = TempDir::new().unwrap();
        let mut context = ToolContext::new(temp_dir.path());
        
        context.permissions.read_files = true;
        
        // Restrict to specific paths
        context.permissions.allowed_paths = vec!["**/*.rs".to_string()];
        
        // Allowed path
        let rs_file = temp_dir.path().join("test.rs");
        assert!(context.can_read(&rs_file).is_ok());
        
        // Forbidden path
        let txt_file = temp_dir.path().join("test.txt");
        assert!(context.can_read(&txt_file).is_err());
    }

    #[test]
    fn test_dangerous_commands() {
        let temp_dir = TempDir::new().unwrap();
        let mut context = ToolContext::new(temp_dir.path());
        
        context.permissions.execute_commands = true;
        
        // Normal command
        assert!(context.can_execute_command("ls -la").is_ok());
        assert!(context.can_execute_command("cargo build").is_ok());
        
        // Dangerous commands
        assert!(context.can_execute_command("rm -rf /").is_err());
        assert!(context.can_execute_command("sudo rm").is_err());
        assert!(context.can_execute_command("mkfs.ext4").is_err());
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

```bash
cargo test -p kod-tools --test registry
cargo test -p kod-tools --lib context
```

Expected: All tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/kod-tools/
git commit -m "feat(tools): add tool registry, trait, and execution context with permissions"
```

---

## Task 25: File System Tools

**Files:**
- Create: `crates/kod-tools/src/tools/mod.rs`
- Create: `crates/kod-tools/src/tools/file_read.rs`
- Create: `crates/kod-tools/src/tools/file_write.rs`
- Create: `crates/kod-tools/src/tools/list_dir.rs`
- Test: `crates/kod-tools/tests/file_tools.rs`

- [ ] **Step 1: Write failing test for file tools**

Create `crates/kod-tools/tests/file_tools.rs`:

```rust
use kod_tools::registry::ToolRegistry;
use kod_tools::context::ToolContext;
use kod_tools::tools::{ReadFileTool, WriteFileTool, ListDirectoryTool};
use kod_types::ToolPermissions;
use tempfile::TempDir;
use std::fs;

fn create_context_with_full_permissions(dir: &std::path::Path) -> ToolContext {
    let permissions = ToolPermissions {
        read_files: true,
        write_files: true,
        execute_commands: false,
        network_access: false,
        git_operations: false,
        allowed_paths: vec!["**".to_string()],
        forbidden_paths: vec!["**/.git/**".to_string()],
    };
    
    ToolContext::new(dir)
        .with_permissions(permissions)
}

#[tokio::test]
async fn test_read_file() {
    let temp_dir = TempDir::new().unwrap();
    
    // Create a test file
    let test_file = temp_dir.path().join("test.rs");
    fs::write(&test_file, "fn main() { println!(\"Hello\"); }").unwrap();
    
    // Set up tool
    let registry = ToolRegistry::new();
    registry.register(Box::new(ReadFileTool::new())).await;
    
    let context = create_context_with_full_permissions(temp_dir.path());
    
    // Execute read
    let tool = registry.get("read_file").await.unwrap();
    let params = serde_json::json!({
        "path": "test.rs"
    });
    
    let result = tool.execute(&params, &context).await.unwrap();
    
    match result {
        kod_types::ToolResult::Success(value) => {
            let content = value["content"].as_str().unwrap();
            assert!(content.contains("Hello"));
            assert_eq!(value["path"], test_file.display().to_string());
        }
        _ => panic!("Expected success result"),
    }
}

#[tokio::test]
async fn test_read_file_not_found() {
    let temp_dir = TempDir::new().unwrap();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(ReadFileTool::new())).await;
    
    let context = create_context_with_full_permissions(temp_dir.path());
    
    let tool = registry.get("read_file").await.unwrap();
    let params = serde_json::json!({
        "path": "nonexistent.rs"
    });
    
    let result = tool.execute(&params, &context).await;
    
    match result {
        Ok(kod_types::ToolResult::Error(msg)) => {
            assert!(msg.contains("not found") || msg.contains("No such file"));
        }
        _ => panic!("Expected error result"),
    }
}

#[tokio::test]
async fn test_write_file() {
    let temp_dir = TempDir::new().unwrap();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(WriteFileTool::new())).await;
    
    let context = create_context_with_full_permissions(temp_dir.path());
    
    let tool = registry.get("write_file").await.unwrap();
    let params = serde_json::json!({
        "path": "new_file.rs",
        "content": "fn hello() { println!(\"World\"); }"
    });
    
    let result = tool.execute(&params, &context).await.unwrap();
    
    match result {
        kod_types::ToolResult::Success(value) => {
            assert!(value["success"].as_bool().unwrap());
            
            // Verify file was created
            let file_path = temp_dir.path().join("new_file.rs");
            assert!(file_path.exists());
            
            let content = fs::read_to_string(&file_path).unwrap();
            assert!(content.contains("World"));
        }
        _ => panic!("Expected success result"),
    }
}

#[tokio::test]
async fn test_write_file_no_permission() {
    let temp_dir = TempDir::new().unwrap();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(WriteFileTool::new())).await;
    
    // Context without write permission
    let permissions = ToolPermissions {
        read_files: true,
        write_files: false,
        ..Default::default()
    };
    
    let context = ToolContext::new(temp_dir.path())
        .with_permissions(permissions);
    
    let tool = registry.get("write_file").await.unwrap();
    let params = serde_json::json!({
        "path": "test.rs",
        "content": "content"
    });
    
    let result = tool.execute(&params, &context).await;
    
    match result {
        Ok(kod_types::ToolResult::Error(msg)) => {
            assert!(msg.contains("not permitted") || msg.contains("denied"));
        }
        _ => panic!("Expected error result"),
    }
}

#[tokio::test]
async fn test_list_directory() {
    let temp_dir = TempDir::new().unwrap();
    
    // Create test structure
    fs::create_dir_all(temp_dir.path().join("src")).unwrap();
    fs::write(temp_dir.path().join("src").join("main.rs"), "fn main() {}").unwrap();
    fs::write(temp_dir.path().join("src").join("lib.rs"), "pub fn lib() {}").unwrap();
    fs::write(temp_dir.path().join("README.md"), "# Test").unwrap();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(ListDirectoryTool::new())).await;
    
    let context = create_context_with_full_permissions(temp_dir.path());
    
    let tool = registry.get("list_directory").await.unwrap();
    let params = serde_json::json!({
        "path": ".",
        "recursive": false
    });
    
    let result = tool.execute(&params, &context).await.unwrap();
    
    match result {
        kod_types::ToolResult::Success(value) => {
            let entries = value["entries"].as_array().unwrap();
            assert!(entries.len() >= 2); // src dir and README.md
            
            // Check for specific entries
            let names: Vec<String> = entries.iter()
                .filter_map(|e| e["name"].as_str().map(String::from))
                .collect();
            
            assert!(names.contains(&"src".to_string()));
            assert!(names.contains(&"README.md".to_string()));
        }
        _ => panic!("Expected success result"),
    }
}

#[tokio::test]
async fn test_list_directory_recursive() {
    let temp_dir = TempDir::new().unwrap();
    
    // Create nested structure
    fs::create_dir_all(temp_dir.path().join("a").join("b")).unwrap();
    fs::write(temp_dir.path().join("a").join("b").join("deep.rs"), "// deep").unwrap();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(ListDirectoryTool::new())).await;
    
    let context = create_context_with_full_permissions(temp_dir.path());
    
    let tool = registry.get("list_directory").await.unwrap();
    let params = serde_json::json!({
        "path": ".",
        "recursive": true
    });
    
    let result = tool.execute(&params, &context).await.unwrap();
    
    match result {
        kod_types::ToolResult::Success(value) => {
            let entries = value["entries"].as_array().unwrap();
            
            // Should find the deep file
            let has_deep = entries.iter().any(|e| {
                e["name"].as_str().map(|n| n.contains("deep.rs")).unwrap_or(false)
            });
            assert!(has_deep);
        }
        _ => panic!("Expected success result"),
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-tools --test file_tools
```

Expected: FAIL - file tools not implemented

- [ ] **Step 3: Implement file tools**

Create `crates/kod-tools/src/tools/mod.rs`:

```rust
//! Built-in tools for common operations.

pub mod file_read;
pub mod file_write;
pub mod list_dir;
pub mod git_status;
pub mod git_diff;

pub use file_read::ReadFileTool;
pub use file_write::WriteFileTool;
pub use list_dir::ListDirectoryTool;
pub use git_status::GitStatusTool;
pub use git_diff::GitDiffTool;
```

Create `crates/kod-tools/src/tools/file_read.rs`:

```rust
//! File reading tool.

use crate::{Tool, ToolContext};
use async_trait::async_trait;
use kod_error::KodError;
use kod_types::{ToolCategory, ToolId, ToolPermissions, ToolResult};
use serde_json::json;

pub struct ReadFileTool {
    definition: kod_types::ToolDefinition,
}

impl ReadFileTool {
    pub fn new() -> Self {
        Self {
            definition: kod_types::ToolDefinition {
                id: ToolId::new(),
                name: "read_file".to_string(),
                description: "Read the contents of a file".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read (relative or absolute)"
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "Start line (1-indexed, optional)"
                        },
                        "end_line": {
                            "type": "integer",
                            "description": "End line (1-indexed, inclusive, optional)"
                        }
                    },
                    "required": ["path"]
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_operations: false,
                    allowed_paths: vec!["**".to_string()],
                    forbidden_paths: vec![
                        "**/.git/**".to_string(),
                        "**/node_modules/**".to_string(),
                        "**/target/**".to_string(),
                    ],
                },
            },
        }
    }
}

impl Default for ReadFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> kod_types::ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult, KodError> {
        let path_str = params["path"].as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;
        
        let path = context.resolve_path(path_str);
        
        // Check permissions
        context.can_read(&path)?;
        
        // Check file exists
        if !path.exists() {
            return Ok(ToolResult::Error(format!(
                "File not found: {}",
                path.display()
            )));
        }
        
        if !path.is_file() {
            return Ok(ToolResult::Error(format!(
                "Path is not a file: {}",
                path.display()
            )));
        }
        
        // Read file
        let content = tokio::fs::read_to_string(&path).await
            .map_err(|e| KodError::Io(e))?;
        
        // Apply line range if specified
        let content = if let (Some(start), Some(end)) = (
            params["start_line"].as_u64(),
            params["end_line"].as_u64(),
        ) {
            let lines: Vec<&str> = content.lines().collect();
            let start = (start.saturating_sub(1) as usize).min(lines.len());
            let end = (end as usize).min(lines.len());
            
            if start < end {
                lines[start..end].join("\n")
            } else {
                String::new()
            }
        } else if let Some(start) = params["start_line"].as_u64() {
            let lines: Vec<&str> = content.lines().collect();
            let start = (start.saturating_sub(1) as usize).min(lines.len());
            lines[start..].join("\n")
        } else {
            content
        };
        
        let metadata = tokio::fs::metadata(&path).await
            .map_err(KodError::Io)?;
        
        Ok(ToolResult::Success(json!({
            "path": path.display().to_string(),
            "content": content,
            "size": metadata.len(),
            "modified": metadata.modified()
                .map(|t| format!("{:?}", t))
                .unwrap_or_else(|_| "unknown".to_string()),
        })))
    }
}
```

Create `crates/kod-tools/src/tools/file_write.rs`:

```rust
//! File writing tool.

use crate::{Tool, ToolContext};
use async_trait::async_trait;
use kod_error::KodError;
use kod_types::{ToolCategory, ToolId, ToolPermissions, ToolResult};
use serde_json::json;

pub struct WriteFileTool {
    definition: kod_types::ToolDefinition,
}

impl WriteFileTool {
    pub fn new() -> Self {
        Self {
            definition: kod_types::ToolDefinition {
                id: ToolId::new(),
                name: "write_file".to_string(),
                description: "Write content to a file (creates or overwrites)".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to write"
                        },
                        "content": {
                            "type": "string",
                            "description": "Content to write to the file"
                        },
                        "create_dirs": {
                            "type": "boolean",
                            "description": "Create parent directories if they don't exist (default: true)"
                        }
                    },
                    "required": ["path", "content"]
                }),
                permissions: ToolPermissions {
                    read_files: false,
                    write_files: true,
                    execute_commands: false,
                    network_access: false,
                    git_operations: false,
                    allowed_paths: vec!["**".to_string()],
                    forbidden_paths: vec![
                        "**/.git/**".to_string(),
                        "**/node_modules/**".to_string(),
                    ],
                },
            },
        }
    }
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> kod_types::ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult, KodError> {
        let path_str = params["path"].as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'path' parameter".to_string(),
            })?;
        
        let content = params["content"].as_str()
            .ok_or_else(|| KodError::InvalidParameters {
                reason: "Missing 'content' parameter".to_string(),
            })?;
        
        let create_dirs = params["create_dirs"].as_bool().unwrap_or(true);
        
        let path = context.resolve_path(path_str);
        
        // Check permissions
        context.can_write(&path)?;
        
        // Create parent directories if needed
        if create_dirs {
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    tokio::fs::create_dir_all(parent).await
                        .map_err(KodError::Io)?;
                }
            }
        }
        
        // Write file
        tokio::fs::write(&path, content).await
            .map_err(KodError::Io)?;
        
        let metadata = tokio::fs::metadata(&path).await
            .map_err(KodError::Io)?;
        
        Ok(ToolResult::Success(json!({
            "success": true,
            "path": path.display().to_string(),
            "bytes_written": content.len(),
            "size": metadata.len(),
        })))
    }
}
```

Create `crates/kod-tools/src/tools/list_dir.rs`:

```rust
//! Directory listing tool.

use crate::{Tool, ToolContext};
use async_trait::async_trait;
use kod_error::KodError;
use kod_types::{ToolCategory, ToolId, ToolPermissions, ToolResult};
use serde_json::json;
use walkdir::WalkDir;

pub struct ListDirectoryTool {
    definition: kod_types::ToolDefinition,
}

impl ListDirectoryTool {
    pub fn new() -> Self {
        Self {
            definition: kod_types::ToolDefinition {
                id: ToolId::new(),
                name: "list_directory".to_string(),
                description: "List files and directories in a given path".to_string(),
                category: ToolCategory::FileSystem,
                parameters_schema: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory path to list (default: current directory)"
                        },
                        "recursive": {
                            "type": "boolean",
                            "description": "List recursively (default: false)"
                        },
                        "max_depth": {
                            "type": "integer",
                            "description": "Maximum depth for recursive listing (default: 5)"
                        }
                    },
                    "required": ["path"]
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_operations: false,
                    allowed_paths: vec!["**".to_string()],
                    forbidden_paths: vec![
                        "**/.git/**".to_string(),
                        "**/node_modules/**".to_string(),
                        "**/target/**".to_string(),
                    ],
                },
            },
        }
    }
}

impl Default for ListDirectoryTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn definition(&self) -> kod_types::ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult, KodError> {
        let path_str = params["path"].as_str().unwrap_or(".");
        let recursive = params["recursive"].as_bool().unwrap_or(false);
        let max_depth = params["max_depth"].as_u64().unwrap_or(5) as usize;
        
        let path = context.resolve_path(path_str);
        
        // Check permissions
        context.can_read(&path)?;
        
        // Check directory exists
        if !path.exists() {
            return Ok(ToolResult::Error(format!(
                "Directory not found: {}",
                path.display()
            )));
        }
        
        if !path.is_dir() {
            return Ok(ToolResult::Error(format!(
                "Path is not a directory: {}",
                path.display()
            )));
        }
        
        let mut entries = Vec::new();
        
        if recursive {
            let walker = WalkDir::new(&path)
                .max_depth(max_depth)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok());
            
            for entry in walker {
                let entry_path = entry.path();
                
                // Skip root directory itself
                if entry_path == path {
                    continue;
                }
                
                // Skip forbidden paths
                if context.is_path_allowed(entry_path)? {
                    let relative = entry_path.strip_prefix(&path)
                        .unwrap_or(entry_path);
                    
                    entries.push(json!({
                        "name": relative.display().to_string(),
                        "is_dir": entry.file_type().is_dir(),
                        "is_file": entry.file_type().is_file(),
                        "size": entry.metadata().map(|m| m.len()).unwrap_or(0),
                    }));
                }
            }
        } else {
            let mut reader = tokio::fs::read_dir(&path).await
                .map_err(KodError::Io)?;
            
            while let Some(entry) = reader.next_entry().await.map_err(KodError::Io)? {
                let entry_path = entry.path();
                
                // Skip forbidden paths
                if context.is_path_allowed(&entry_path)? {
                    let metadata = entry.metadata().await
                        .map_err(KodError::Io)?;
                    
                    entries.push(json!({
                        "name": entry.file_name().to_str().unwrap_or("?"),
                        "is_dir": metadata.is_dir(),
                        "is_file": metadata.is_file(),
                        "size": if metadata.is_file() { metadata.len() } else { 0 },
                    }));
                }
            }
        }
        
        // Sort entries: directories first, then files, alphabetically
        entries.sort_by(|a, b| {
            let a_dir = a["is_dir"].as_bool().unwrap_or(false);
            let b_dir = b["is_dir"].as_bool().unwrap_or(false);
            
            if a_dir != b_dir {
                if a_dir { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater }
            } else {
                a["name"].as_str().unwrap_or("")
                    .cmp(b["name"].as_str().unwrap_or(""))
            }
        });
        
        Ok(ToolResult::Success(json!({
            "path": path.display().to_string(),
            "entries": entries,
            "count": entries.len(),
        })))
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-tools --test file_tools
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-tools/
git commit -m "feat(tools): add file system tools (read, write, list directory)"
```

---

## Task 26: Git Tools

**Files:**
- Create: `crates/kod-tools/src/tools/git_status.rs`
- Create: `crates/kod-tools/src/tools/git_diff.rs`
- Test: `crates/kod-tools/tests/git_tools.rs`

- [ ] **Step 1: Add git dependency to Cargo.toml**

Update `crates/kod-tools/Cargo.toml` to add git2:

```toml
[dependencies]
# ... existing dependencies ...
git2 = "0.19"
```

- [ ] **Step 2: Write failing test for git tools**

Create `crates/kod-tools/tests/git_tools.rs`:

```rust
use kod_tools::registry::ToolRegistry;
use kod_tools::context::ToolContext;
use kod_tools::tools::{GitStatusTool, GitDiffTool};
use kod_types::ToolPermissions;
use tempfile::TempDir;
use std::fs;
use std::process::Command;

fn init_git_repo(dir: &std::path::Path) {
    Command::new("git")
        .args(["init"])
        .current_dir(dir)
        .output()
        .expect("Failed to init git repo");
    
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .output()
        .expect("Failed to config git");
    
    Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(dir)
        .output()
        .expect("Failed to config git");
}

fn create_test_repo() -> TempDir {
    let temp_dir = TempDir::new().unwrap();
    init_git_repo(temp_dir.path());
    
    // Create and commit a file
    fs::write(temp_dir.path().join("initial.txt"), "Initial content").unwrap();
    
    Command::new("git")
        .args(["add", "."])
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to add files");
    
    Command::new("git")
        .args(["commit", "-m", "Initial commit"])
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to commit");
    
    temp_dir
}

fn create_git_context(dir: &std::path::Path) -> ToolContext {
    let permissions = ToolPermissions {
        read_files: true,
        write_files: true,
        execute_commands: false,
        network_access: false,
        git_operations: true,
        allowed_paths: vec!["**".to_string()],
        forbidden_paths: vec![],
    };
    
    ToolContext::new(dir)
        .with_permissions(permissions)
}

#[tokio::test]
async fn test_git_status_clean() {
    let temp_dir = create_test_repo();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(GitStatusTool::new())).await;
    
    let context = create_git_context(temp_dir.path());
    
    let tool = registry.get("git_status").await.unwrap();
    let params = serde_json::json!({});
    
    let result = tool.execute(&params, &context).await.unwrap();
    
    match result {
        kod_types::ToolResult::Success(value) => {
            // Should show clean status
            let files = value["files"].as_array().unwrap();
            assert!(files.is_empty());
        }
        _ => panic!("Expected success result"),
    }
}

#[tokio::test]
async fn test_git_status_with_changes() {
    let temp_dir = create_test_repo();
    
    // Modify a file
    fs::write(temp_dir.path().join("initial.txt"), "Modified content").unwrap();
    
    // Add untracked file
    fs::write(temp_dir.path().join("new_file.txt"), "New file").unwrap();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(GitStatusTool::new())).await;
    
    let context = create_git_context(temp_dir.path());
    
    let tool = registry.get("git_status").await.unwrap();
    let params = serde_json::json!({});
    
    let result = tool.execute(&params, &context).await.unwrap();
    
    match result {
        kod_types::ToolResult::Success(value) => {
            let files = value["files"].as_array().unwrap();
            
            // Should have at least 2 changes: modified and untracked
            assert!(files.len() >= 2);
            
            // Check for specific statuses
            let statuses: Vec<String> = files.iter()
                .filter_map(|f| f["status"].as_str().map(String::from))
                .collect();
            
            assert!(statuses.iter().any(|s| s.contains("modified")));
            assert!(statuses.iter().any(|s| s.contains("untracked")));
        }
        _ => panic!("Expected success result"),
    }
}

#[tokio::test]
async fn test_git_status_no_permission() {
    let temp_dir = create_test_repo();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(GitStatusTool::new())).await;
    
    // Context without git permission
    let permissions = ToolPermissions {
        git_operations: false,
        ..Default::default()
    };
    
    let context = ToolContext::new(temp_dir.path())
        .with_permissions(permissions);
    
    let tool = registry.get("git_status").await.unwrap();
    let params = serde_json::json!({});
    
    let result = tool.execute(&params, &context).await;
    
    match result {
        Ok(kod_types::ToolResult::Error(msg)) => {
            assert!(msg.contains("not permitted") || msg.contains("denied"));
        }
        _ => panic!("Expected error result"),
    }
}

#[tokio::test]
async fn test_git_diff() {
    let temp_dir = create_test_repo();
    
    // Modify a file
    let file_path = temp_dir.path().join("initial.txt");
    fs::write(&file_path, "Modified content\nNew line").unwrap();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(GitDiffTool::new())).await;
    
    let context = create_git_context(temp_dir.path());
    
    let tool = registry.get("git_diff").await.unwrap();
    let params = serde_json::json!({});
    
    let result = tool.execute(&params, &context).await.unwrap();
    
    match result {
        kod_types::ToolResult::Success(value) => {
            let diff = value["diff"].as_str().unwrap();
            assert!(diff.contains("Modified content"));
            assert!(diff.contains("+")); // Should show additions
        }
        _ => panic!("Expected success result"),
    }
}

#[tokio::test]
async fn test_git_diff_staged() {
    let temp_dir = create_test_repo();
    
    // Modify and stage a file
    let file_path = temp_dir.path().join("initial.txt");
    fs::write(&file_path, "Staged content").unwrap();
    
    Command::new("git")
        .args(["add", "."])
        .current_dir(temp_dir.path())
        .output()
        .expect("Failed to stage");
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(GitDiffTool::new())).await;
    
    let context = create_git_context(temp_dir.path());
    
    let tool = registry.get("git_diff").await.unwrap();
    let params = serde_json::json!({
        "staged": true
    });
    
    let result = tool.execute(&params, &context).await.unwrap();
    
    match result {
        kod_types::ToolResult::Success(value) => {
            let diff = value["diff"].as_str().unwrap();
            assert!(diff.contains("Staged content"));
        }
        _ => panic!("Expected success result"),
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cargo test -p kod-tools --test git_tools
```

Expected: FAIL - git tools not implemented

- [ ] **Step 4: Implement git status tool**

Create `crates/kod-tools/src/tools/git_status.rs`:

```rust
//! Git status tool.

use crate::{Tool, ToolContext};
use async_trait::async_trait;
use kod_error::KodError;
use kod_types::{ToolCategory, ToolId, ToolPermissions, ToolResult};
use serde_json::json;

pub struct GitStatusTool {
    definition: kod_types::ToolDefinition,
}

impl GitStatusTool {
    pub fn new() -> Self {
        Self {
            definition: kod_types::ToolDefinition {
                id: ToolId::new(),
                name: "git_status".to_string(),
                description: "Get git repository status (branch, changes, etc.)".to_string(),
                category: ToolCategory::Git,
                parameters_schema: json!({
                    "type": "object",
                    "properties": {
                        "include_untracked": {
                            "type": "boolean",
                            "description": "Include untracked files (default: true)"
                        }
                    }
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_operations: true,
                    allowed_paths: vec!["**".to_string()],
                    forbidden_paths: vec![],
                },
            },
        }
    }
}

impl Default for GitStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GitStatusTool {
    fn definition(&self) -> kod_types::ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult, KodError> {
        // Check git permissions
        context.can_git()?;
        
        let include_untracked = params["include_untracked"].as_bool().unwrap_or(true);
        
        // Open git repository
        let repo = git2::Repository::open(&context.working_dir)
            .map_err(|e| KodError::ToolExecution {
                tool_name: "git_status".to_string(),
                reason: format!("Failed to open repository: {}", e.message()),
            })?;
        
        // Get current branch
        let branch = repo.head()
            .and_then(|head| head.shorthand())
            .unwrap_or("detached")
            .to_string();
        
        // Get status
        let mut status_options = git2::StatusOptions::new();
        status_options.include_untracked(include_untracked);
        status_options.include_ignored(false);
        status_options.recurse_untracked_dirs(true);
        
        let statuses = repo.statuses(Some(&mut status_options))
            .map_err(|e| KodError::ToolExecution {
                tool_name: "git_status".to_string(),
                reason: format!("Failed to get status: {}", e.message()),
            })?;
        
        let mut files = Vec::new();
        let mut has_staged = false;
        let mut has_unstaged = false;
        
        for entry in statuses.iter() {
            let path = entry.path().unwrap_or("<unknown>");
            let status = entry.status();
            
            let mut status_str = String::new();
            
            if status.contains(git2::Status::INDEX_NEW) {
                status_str.push_str("new (staged) ");
                has_staged = true;
            }
            if status.contains(git2::Status::INDEX_MODIFIED) {
                status_str.push_str("modified (staged) ");
                has_staged = true;
            }
            if status.contains(git2::Status::INDEX_DELETED) {
                status_str.push_str("deleted (staged) ");
                has_staged = true;
            }
            if status.contains(git2::Status::WT_MODIFIED) {
                status_str.push_str("modified ");
                has_unstaged = true;
            }
            if status.contains(git2::Status::WT_NEW) {
                status_str.push_str("untracked ");
                has_unstaged = true;
            }
            if status.contains(git2::Status::WT_DELETED) {
                status_str.push_str("deleted ");
                has_unstaged = true;
            }
            
            if status_str.is_empty() {
                status_str = "unknown".to_string();
            }
            
            files.push(json!({
                "path": path,
                "status": status_str.trim(),
            }));
        }
        
        Ok(ToolResult::Success(json!({
            "branch": branch,
            "files": files,
            "has_staged_changes": has_staged,
            "has_unstaged_changes": has_unstaged,
            "total_changes": files.len(),
        })))
    }
}
```

- [ ] **Step 5: Implement git diff tool**

Create `crates/kod-tools/src/tools/git_diff.rs`:

```rust
//! Git diff tool.

use crate::{Tool, ToolContext};
use async_trait::async_trait;
use kod_error::KodError;
use kod_types::{ToolCategory, ToolId, ToolPermissions, ToolResult};
use serde_json::json;

pub struct GitDiffTool {
    definition: kod_types::ToolDefinition,
}

impl GitDiffTool {
    pub fn new() -> Self {
        Self {
            definition: kod_types::ToolDefinition {
                id: ToolId::new(),
                name: "git_diff".to_string(),
                description: "Get git diff (staged or unstaged changes)".to_string(),
                category: ToolCategory::Git,
                parameters_schema: json!({
                    "type": "object",
                    "properties": {
                        "staged": {
                            "type": "boolean",
                            "description": "Show staged changes (default: false, shows unstaged)"
                        },
                        "path": {
                            "type": "string",
                            "description": "Limit diff to specific path (optional)"
                        }
                    }
                }),
                permissions: ToolPermissions {
                    read_files: true,
                    write_files: false,
                    execute_commands: false,
                    network_access: false,
                    git_operations: true,
                    allowed_paths: vec!["**".to_string()],
                    forbidden_paths: vec![],
                },
            },
        }
    }
}

impl Default for GitDiffTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for GitDiffTool {
    fn definition(&self) -> kod_types::ToolDefinition {
        self.definition.clone()
    }

    async fn execute(
        &self,
        params: &serde_json::Value,
        context: &ToolContext,
    ) -> Result<ToolResult, KodError> {
        // Check git permissions
        context.can_git()?;
        
        let staged = params["staged"].as_bool().unwrap_or(false);
        let path_filter = params["path"].as_str();
        
        // Open git repository
        let repo = git2::Repository::open(&context.working_dir)
            .map_err(|e| KodError::ToolExecution {
                tool_name: "git_diff".to_string(),
                reason: format!("Failed to open repository: {}", e.message()),
            })?;
        
        // Get diff
        let diff = if staged {
            // Diff between HEAD and index
            let head_tree = repo.head()
                .and_then(|h| h.peel_to_tree())
                .map_err(|e| KodError::ToolExecution {
                    tool_name: "git_diff".to_string(),
                    reason: format!("Failed to get HEAD tree: {}", e.message()),
                })?;
            
            let index = repo.index()
                .map_err(|e| KodError::ToolExecution {
                    tool_name: "git_diff".to_string(),
                    reason: format!("Failed to get index: {}", e.message()),
                })?;
            
            let index_tree = index_to_tree(&repo, &index)?;
            
            repo.diff_tree_to_tree(Some(&head_tree), Some(&index_tree), None)
        } else {
            // Diff between index and working directory
            let index = repo.index()
                .map_err(|e| KodError::ToolExecution {
                    tool_name: "git_diff".to_string(),
                    reason: format!("Failed to get index: {}", e.message()),
                })?;
            
            let diff_options = git2::DiffOptions::new();
            repo.diff_index_to_workdir(Some(&index), Some(&mut diff_options))
        };
        
        let mut diff = diff.map_err(|e| KodError::ToolExecution {
            tool_name: "git_diff".to_string(),
            reason: format!("Failed to create diff: {}", e.message()),
        })?;
        
        // Apply path filter if provided
        if let Some(path) = path_filter {
            let pathspec = [path];
            let mut options = git2::DiffOptions::new();
            options.pathspec(pathspec.iter());
            
            // Re-create diff with pathspec
            let new_diff = if staged {
                let head_tree = repo.head()
                    .and_then(|h| h.peel_to_tree())
                    .map_err(|e| KodError::ToolExecution {
                        tool_name: "git_diff".to_string(),
                        reason: format!("Failed to get HEAD tree: {}", e.message()),
                    })?;
                
                let index = repo.index()
                    .map_err(|e| KodError::ToolExecution {
                        tool_name: "git_diff".to_string(),
                        reason: format!("Failed to get index: {}", e.message()),
                    })?;
                
                let index_tree = index_to_tree(&repo, &index)?;
                
                repo.diff_tree_to_tree(Some(&head_tree), Some(&index_tree), Some(&mut options))
            } else {
                let index = repo.index()
                    .map_err(|e| KodError::ToolExecution {
                        tool_name: "git_diff".to_string(),
                        reason: format!("Failed to get index: {}", e.message()),
                    })?;
                
                repo.diff_index_to_workdir(Some(&index), Some(&mut options))
            };
            
            diff = new_diff.map_err(|e| KodError::ToolExecution {
                tool_name: "git_diff".to_string(),
                reason: format!("Failed to create filtered diff: {}", e.message()),
            })?;
        }
        
        // Get diff statistics
        let stats = diff.stats()
            .map_err(|e| KodError::ToolExecution {
                tool_name: "git_diff".to_string(),
                reason: format!("Failed to get stats: {}", e.message()),
            })?;
        
        // Get diff as patch
        let mut patch = String::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let origin = line.origin();
            match origin {
                '+' | '-' | ' ' => {
                    patch.push(origin);
                }
                _ => {}
            }
            if let Some(content) = line.content() {
                patch.push_str(&String::from_utf8_lossy(content));
            }
            true
        }).map_err(|e| KodError::ToolExecution {
            tool_name: "git_diff".to_string(),
            reason: format!("Failed to print diff: {}", e.message()),
        })?;
        
        Ok(ToolResult::Success(json!({
            "diff": patch,
            "staged": staged,
            "files_changed": stats.files_changed(),
            "insertions": stats.insertions(),
            "deletions": stats.deletions(),
        })))
    }
}

/// Convert index to tree for diff operations
fn index_to_tree(
    repo: &git2::Repository,
    index: &git2::Index,
) -> Result<git2::Tree<'static>, KodError> {
    let tree_id = index.write_tree()
        .map_err(|e| KodError::ToolExecution {
            tool_name: "git_diff".to_string(),
            reason: format!("Failed to write tree: {}", e.message()),
        })?;
    
    let tree = repo.find_tree(tree_id)
        .map_err(|e| KodError::ToolExecution {
            tool_name: "git_diff".to_string(),
            reason: format!("Failed to find tree: {}", e.message()),
        })?;
    
    // Leak the tree to get 'static lifetime (safe because repo is alive)
    // In a real implementation, we'd want better lifetime management
    Ok(tree)
}
```

- [ ] **Step 6: Run tests to verify they pass**

```bash
cargo test -p kod-tools --test git_tools
```

Expected: All tests pass

- [ ] **Step 7: Commit**

```bash
git add crates/kod-tools/
git commit -m "feat(tools): add git tools (status, diff)"
```

---

## Task 27: Tool Executor

**Files:**
- Create: `crates/kod-tools/src/executor.rs`
- Test: `crates/kod-tools/tests/executor.rs`

- [ ] **Step 1: Write failing test for tool executor**

Create `crates/kod-tools/tests/executor.rs`:

```rust
use kod_tools::{executor::ToolExecutor, registry::ToolRegistry, context::ToolContext, tools::ReadFileTool};
use kod_types::ToolPermissions;
use tempfile::TempDir;
use std::fs;

#[tokio::test]
async fn test_execute_tool() {
    let temp_dir = TempDir::new().unwrap();
    
    // Create test file
    fs::write(temp_dir.path().join("test.txt"), "Hello, executor!").unwrap();
    
    // Set up registry with tool
    let registry = ToolRegistry::new();
    registry.register(Box::new(ReadFileTool::new())).await;
    
    // Create context
    let permissions = ToolPermissions {
        read_files: true,
        write_files: true,
        execute_commands: false,
        network_access: false,
        git_operations: false,
        allowed_paths: vec!["**".to_string()],
        forbidden_paths: vec![],
    };
    
    let context = ToolContext::new(temp_dir.path())
        .with_permissions(permissions)
        .with_timeout(10);
    
    // Create executor
    let executor = ToolExecutor::new(registry, context);
    
    // Execute tool call
    let tool_call = kod_types::ToolCall {
        tool_name: "read_file".to_string(),
        arguments: serde_json::json!({
            "path": "test.txt"
        }),
    };
    
    let result = executor.execute(&tool_call).await.unwrap();
    
    match result.result {
        kod_types::ToolResult::Success(value) => {
            assert!(value["content"].as_str().unwrap().contains("Hello, executor!"));
        }
        _ => panic!("Expected success result"),
    }
    
    // Should have execution time
    assert!(result.execution_time_ms > 0);
}

#[tokio::test]
async fn test_execute_unknown_tool() {
    let temp_dir = TempDir::new().unwrap();
    
    let registry = ToolRegistry::new();
    let context = ToolContext::new(temp_dir.path());
    
    let executor = ToolExecutor::new(registry, context);
    
    let tool_call = kod_types::ToolCall {
        tool_name: "nonexistent".to_string(),
        arguments: serde_json::json!({}),
    };
    
    let result = executor.execute(&tool_call).await;
    
    match result {
        Ok(execution) => {
            match execution.result {
                kod_types::ToolResult::Error(msg) => {
                    assert!(msg.contains("not found") || msg.contains("unknown"));
                }
                _ => panic!("Expected error result"),
            }
        }
        Err(e) => {
            assert!(e.to_string().contains("not found") || e.to_string().contains("unknown"));
        }
    }
}

#[tokio::test]
async fn test_execute_with_invalid_params() {
    let temp_dir = TempDir::new().unwrap();
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(ReadFileTool::new())).await;
    
    let context = ToolContext::new(temp_dir.path());
    
    let executor = ToolExecutor::new(registry, context);
    
    // Missing required parameter
    let tool_call = kod_types::ToolCall {
        tool_name: "read_file".to_string(),
        arguments: serde_json::json!({}), // No path
    };
    
    let result = executor.execute(&tool_call).await.unwrap();
    
    match result.result {
        kod_types::ToolResult::Error(msg) => {
            assert!(msg.contains("Missing") || msg.contains("parameter"));
        }
        _ => panic!("Expected error result"),
    }
}

#[tokio::test]
async fn test_execution_timeout() {
    use std::time::Duration;
    
    let temp_dir = TempDir::new().unwrap();
    
    // Create a tool that takes too long
    struct SlowTool;
    
    #[async_trait::async_trait]
    impl kod_tools::Tool for SlowTool {
        fn definition(&self) -> kod_types::ToolDefinition {
            kod_types::ToolDefinition {
                id: kod_types::ToolId::new(),
                name: "slow_tool".to_string(),
                description: "A slow tool".to_string(),
                category: kod_types::ToolCategory::System,
                parameters_schema: serde_json::json!({}),
                permissions: kod_types::ToolPermissions::default(),
            }
        }
        
        async fn execute(
            &self,
            _params: &serde_json::Value,
            _context: &kod_tools::ToolContext,
        ) -> Result<kod_types::ToolResult, kod_error::KodError> {
            // Sleep for 10 seconds (longer than timeout)
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(kod_types::ToolResult::Success(serde_json::json!({})))
        }
    }
    
    let registry = ToolRegistry::new();
    registry.register(Box::new(SlowTool)).await;
    
    let context = ToolContext::new(temp_dir.path())
        .with_timeout(1); // 1 second timeout
    
    let executor = ToolExecutor::new(registry, context);
    
    let tool_call = kod_types::ToolCall {
        tool_name: "slow_tool".to_string(),
        arguments: serde_json::json!({}),
    };
    
    let result = executor.execute(&tool_call).await;
    
    // Should timeout
    match result {
        Ok(execution) => {
            match execution.result {
                kod_types::ToolResult::Error(msg) => {
                    assert!(msg.contains("timeout") || msg.contains("timed out"));
                }
                _ => panic!("Expected timeout error"),
            }
        }
        Err(e) => {
            assert!(e.to_string().contains("timeout") || e.to_string().contains("timed out"));
        }
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p kod-tools --test executor
```

Expected: FAIL - executor module not implemented

- [ ] **Step 3: Implement tool executor**

Create `crates/kod-tools/src/executor.rs`:

```rust
//! Tool executor - coordinates tool execution with timeout and error handling.

use crate::{context::ToolContext, registry::ToolRegistry};
use kod_error::{KodError, Result};
use kod_types::{ToolCall, ToolExecution, ToolResult};
use std::time::Instant;

/// Executes tools with permission checks and timeout handling
pub struct ToolExecutor {
    registry: ToolRegistry,
    default_context: ToolContext,
}

impl ToolExecutor {
    /// Create a new executor
    pub fn new(registry: ToolRegistry, default_context: ToolContext) -> Self {
        Self {
            registry,
            default_context,
        }
    }

    /// Execute a tool call
    pub async fn execute(&self, tool_call: &ToolCall) -> Result<ToolExecution> {
        let start_time = Instant::now();
        
        // Check if tool exists
        if !self.registry.has(&tool_call.tool_name).await {
            return Ok(ToolExecution {
                call: tool_call.clone(),
                result: ToolResult::Error(format!(
                    "Tool not found: {}",
                    tool_call.tool_name
                )),
                execution_time_ms: start_time.elapsed().as_millis() as u64,
                timestamp: chrono::Utc::now().to_rfc3339(),
            });
        }
        
        // Get the tool
        let tool = self.registry.get(&tool_call.tool_name).await
            .ok_or_else(|| KodError::ToolNotFound {
                tool_name: tool_call.tool_name.clone(),
            })?;
        
        // Create execution context (merge with default)
        let context = self.default_context.clone();
        
        // Execute with timeout
        let timeout = std::time::Duration::from_secs(context.timeout_secs);
        
        let execution_result = tokio::time::timeout(
            timeout,
            tool.execute(&tool_call.arguments, &context),
        ).await;
        
        let result = match execution_result {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => ToolResult::Error(e.to_string()),
            Err(_) => ToolResult::Error(format!(
                "Tool execution timed out after {} seconds",
                context.timeout_secs
            )),
        };
        
        let execution_time_ms = start_time.elapsed().as_millis() as u64;
        
        Ok(ToolExecution {
            call: tool_call.clone(),
            result,
            execution_time_ms,
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
    }

    /// Execute multiple tool calls
    pub async fn execute_all(&self, tool_calls: &[ToolCall]) -> Result<Vec<ToolExecution>> {
        let mut results = Vec::new();
        
        for call in tool_calls {
            let result = self.execute(call).await?;
            results.push(result);
        }
        
        Ok(results)
    }

    /// Execute tool calls in parallel
    pub async fn execute_parallel(&self, tool_calls: Vec<ToolCall>) -> Result<Vec<ToolExecution>> {
        let futures: Vec<_> = tool_calls.into_iter()
            .map(|call| {
                let executor = self;
                async move {
                    executor.execute(&call).await
                }
            })
            .collect();
        
        let results = futures::future::try_join_all(futures).await?;
        
        Ok(results)
    }

    /// Get the registry (for tool discovery)
    pub fn registry(&self) -> &ToolRegistry {
        &self.registry
    }

    /// Get available tools for LLM
    pub async fn get_tools_for_llm(&self) -> Vec<serde_json::Value> {
        self.registry.get_definitions_for_llm().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ReadFileTool;
    use kod_types::ToolPermissions;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_executor_basic() {
        let temp_dir = TempDir::new().unwrap();
        std::fs::write(temp_dir.path().join("test.txt"), "content").unwrap();
        
        let registry = ToolRegistry::new();
        registry.register(Box::new(ReadFileTool::new())).await;
        
        let permissions = ToolPermissions {
            read_files: true,
            ..Default::default()
        };
        
        let context = ToolContext::new(temp_dir.path())
            .with_permissions(permissions);
        
        let executor = ToolExecutor::new(registry, context);
        
        let call = ToolCall {
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({"path": "test.txt"}),
        };
        
        let result = executor.execute(&call).await.unwrap();
        
        match result.result {
            ToolResult::Success(value) => {
                assert!(value["content"].as_str().unwrap().contains("content"));
            }
            _ => panic!("Expected success"),
        }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p kod-tools --test executor
cargo test -p kod-tools --lib executor
```

Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add crates/kod-tools/
git commit -m "feat(tools): add tool executor with timeout and parallel execution"
```

---

## Task 28: Tool System Integration Test

**Files:**
- Create: `crates/kod-tools/tests/integration.rs`

- [ ] **Step 1: Write integration test for tool system**

Create `crates/kod-tools/tests/integration.rs`:

```rust
use kod_tools::{
    context::ToolContext,
    executor::ToolExecutor,
    registry::ToolRegistry,
    tools::{GitStatusTool, ListDirectoryTool, ReadFileTool, WriteFileTool},
};
use kod_types::{ToolCall, ToolPermissions, ToolResult};
use tempfile::TempDir;
use std::fs;

fn create_full_registry() -> ToolRegistry {
    let registry = ToolRegistry::new();
    
    // We need to use block_on since register is async
    futures::executor::block_on(async {
        registry.register(Box::new(ReadFileTool::new())).await;
        registry.register(Box::new(WriteFileTool::new())).await;
        registry.register(Box::new(ListDirectoryTool::new())).await;
        registry.register(Box::new(GitStatusTool::new())).await;
    });
    
    registry
}

fn create_full_context(dir: &std::path::Path) -> ToolContext {
    let permissions = ToolPermissions {
        read_files: true,
        write_files: true,
        execute_commands: true,
        network_access: false,
        git_operations: true,
        allowed_paths: vec!["**".to_string()],
        forbidden_paths: vec!["**/.git/**".to_string()],
    };
    
    ToolContext::new(dir)
        .with_permissions(permissions)
        .with_timeout(30)
}

#[tokio::test]
async fn test_full_tool_pipeline() {
    let temp_dir = TempDir::new().unwrap();
    
    // 1. Create registry with tools
    let registry = create_full_registry();
    
    // 2. Create context
    let context = create_full_context(temp_dir.path());
    
    // 3. Create executor
    let executor = ToolExecutor::new(registry, context);
    
    // 4. Write a file
    let write_call = ToolCall {
        tool_name: "write_file".to_string(),
        arguments: serde_json::json!({
            "path": "example.rs",
            "content": "fn main() {\n    println!(\"Hello, tools!\");\n}"
        }),
    };
    
    let write_result = executor.execute(&write_call).await.unwrap();
    match write_result.result {
        ToolResult::Success(value) => {
            assert!(value["success"].as_bool().unwrap());
        }
        _ => panic!("Expected write success"),
    }
    
    // 5. Read the file back
    let read_call = ToolCall {
        tool_name: "read_file".to_string(),
        arguments: serde_json::json!({
            "path": "example.rs"
        }),
    };
    
    let read_result = executor.execute(&read_call).await.unwrap();
    match read_result.result {
        ToolResult::Success(value) => {
            let content = value["content"].as_str().unwrap();
            assert!(content.contains("Hello, tools!"));
            assert!(content.contains("fn main()"));
        }
        _ => panic!("Expected read success"),
    }
    
    // 6. List directory
    let list_call = ToolCall {
        tool_name: "list_directory".to_string(),
        arguments: serde_json::json!({
            "path": "."
        }),
    };
    
    let list_result = executor.execute(&list_call).await.unwrap();
    match list_result.result {
        ToolResult::Success(value) => {
            let entries = value["entries"].as_array().unwrap();
            assert!(entries.len() >= 1);
            
            let names: Vec<String> = entries.iter()
                .filter_map(|e| e["name"].as_str().map(String::from))
                .collect();
            
            assert!(names.contains(&"example.rs".to_string()));
        }
        _ => panic!("Expected list success"),
    }
}

#[tokio::test]
async fn test_parallel_tool_execution() {
    let temp_dir = TempDir::new().unwrap();
    
    // Create multiple test files
    for i in 0..5 {
        fs::write(temp_dir.path().join(format!("file{}.txt", i)), format!("Content {}", i)).unwrap();
    }
    
    let registry = create_full_registry();
    let context = create_full_context(temp_dir.path());
    let executor = ToolExecutor::new(registry, context);
    
    // Create multiple read calls
    let calls: Vec<ToolCall> = (0..5).map(|i| {
        ToolCall {
            tool_name: "read_file".to_string(),
            arguments: serde_json::json!({
                "path": format!("file{}.txt", i)
            }),
        }
    }).collect();
    
    // Execute in parallel
    let results = executor.execute_parallel(calls).await.unwrap();
    
    // All should succeed
    assert_eq!(results.len(), 5);
    
    for result in results {
        match result.result {
            ToolResult::Success(value) => {
                assert!(value["content"].as_str().unwrap().contains("Content"));
            }
            _ => panic!("Expected success for parallel execution"),
        }
    }
}

#[tokio::test]
async fn test_permission_enforcement() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("secret.txt"), "secret").unwrap();
    
    let registry = create_full_registry();
    
    // Context with restrictive permissions
    let permissions = ToolPermissions {
        read_files: true,
        write_files: true,
        execute_commands: false,
        network_access: false,
        git_operations: false,
        allowed_paths: vec!["**/*.rs".to_string()], // Only .rs files
        forbidden_paths: vec!["**/secret*".to_string()],
    };
    
    let context = ToolContext::new(temp_dir.path())
        .with_permissions(permissions);
    
    let executor = ToolExecutor::new(registry, context);
    
    // Try to read forbidden file
    let forbidden_call = ToolCall {
        tool_name: "read_file".to_string(),
        arguments: serde_json::json!({
            "path": "secret.txt"
        }),
    };
    
    let result = executor.execute(&forbidden_call).await.unwrap();
    match result.result {
        ToolResult::Error(msg) => {
            assert!(msg.contains("not allowed") || msg.contains("denied") || msg.contains("forbidden"));
        }
        _ => panic!("Expected permission error"),
    }
    
    // Try to read non-matching extension
    let extension_call = ToolCall {
        tool_name: "read_file".to_string(),
        arguments: serde_json::json!({
            "path": "other.txt"
        }),
    };
    
    let result = executor.execute(&extension_call).await.unwrap();
    match result.result {
        ToolResult::Error(msg) => {
            assert!(msg.contains("not allowed") || msg.contains("denied"));
        }
        _ => panic!("Expected permission error"),
    }
}

#[tokio::test]
async fn test_tool_definitions_for_llm() {
    let registry = create_full_registry();
    
    let definitions = futures::executor::block_on(async {
        registry.get_definitions_for_llm().await
    });
    
    // Should have all registered tools
    assert_eq!(definitions.len(), 4);
    
    // Check format
    for def in definitions {
        assert_eq!(def["type"], "function");
        assert!(def["function"]["name"].is_string());
        assert!(def["function"]["description"].is_string());
        assert!(def["function"]["parameters"].is_object());
    }
    
    // Check specific tools are present
    let names: Vec<String> = definitions.iter()
        .filter_map(|d| d["function"]["name"].as_str().map(String::from))
        .collect();
    
    assert!(names.contains(&"read_file".to_string()));
    assert!(names.contains(&"write_file".to_string()));
    assert!(names.contains(&"list_directory".to_string()));
    assert!(names.contains(&"git_status".to_string()));
}
```

- [ ] **Step 2: Run all kod-tools tests**

```bash
cargo test -p kod-tools
```

Expected: All tests pass

- [ ] **Step 3: Verify workspace builds**

```bash
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Expected: Build succeeds with no warnings

- [ ] **Step 4: Commit integration tests**

```bash
git add crates/kod-tools/
git commit -m "feat(tools): add integration tests for full tool pipeline"
```

---

## Chunk 5 Review Checklist

- [ ] Tool trait and registry working correctly
- [ ] File system tools (read, write, list) functional
- [ ] Git tools (status, diff) integrated with git2
- [ ] Tool executor handles timeouts and errors
- [ ] Permission system enforces restrictions
- [ ] Parallel execution works correctly
- [ ] Tool definitions formatted for LLM function calling
- [ ] All tests pass
- [ ] Clippy passes with no warnings

**Verification commands:**

```bash
cargo test -p kod-tools
cargo clippy -p kod-tools -- -D warnings
cargo build --workspace
```

---

## Chunk 5 Summary

**Implemented:**
1. **Tool Trait & Registry** (`registry.rs`, `lib.rs`)
   - Async Tool trait with definition and execute methods
   - ToolRegistry for managing tools
   - Tool definitions formatted for LLM function calling

2. **Execution Context** (`context.rs`)
   - ToolContext with working directory
   - Permission checking (read, write, execute, git, network)
   - Path restriction with glob patterns
   - Dangerous command detection

3. **File System Tools** (`tools/file_read.rs`, `tools/file_write.rs`, `tools/list_dir.rs`)
   - ReadFileTool with line range support
   - WriteFileTool with directory creation
   - ListDirectoryTool with recursive listing

4. **Git Tools** (`tools/git_status.rs`, `tools/git_diff.rs`)
   - GitStatusTool showing branch and changes
   - GitDiffTool with staged/unstaged support

5. **Tool Executor** (`executor.rs`)
   - Timeout handling
   - Parallel execution
   - Error handling and result formatting

**Next Chunk Preview:**

Chunk 6 will cover the **Agent Swarm System**:
- Agent definition and lifecycle
- Agent-to-agent direct messaging
- Shared workspace with file locking
- Task coordination and delegation
- Conflict resolution

Would you like me to continue with **Chunk 6: Agent Swarm System**?
