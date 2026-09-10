use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use wasmi::{Engine, Linker, Module, Store, Config};
use base64::{Engine as _, engine::general_purpose};

#[derive(Error, Debug)]
pub enum WasmTransformerError {
    #[error("Base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("WAT decode error: {0}")]
    Wat(#[from] wat::Error),
    #[error("WASMI error: {0}")]
    Wasmi(#[from] wasmi::Error),
    #[error("WASM missing required export 'transform'")]
    MissingExport,
    #[error("Function execution error: {0}")]
    Execution(String),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WasmTransformerArgs {
    pub base64_wasm: Option<String>,
    pub wat: Option<String>,
    pub input: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WasmTransformerResult {
    pub output: i32,
    pub fuel_consumed: u64,
}

#[derive(Clone, Debug)]
pub struct WasmTransformer {
    fuel_limit: u64,
}

impl Default for WasmTransformer {
    fn default() -> Self {
        Self { fuel_limit: 100_000 }
    }
}

impl WasmTransformer {
    pub fn new(fuel_limit: u64) -> Self {
        Self { fuel_limit }
    }
}

impl Tool for WasmTransformer {
    const NAME: &'static str = "wasm_transformer";

    type Error = WasmTransformerError;
    type Args = WasmTransformerArgs;
    type Output = WasmTransformerResult;

    async fn definition(&self, _prompt: String) -> rig::completion::ToolDefinition {
        rig::completion::ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Execute a lightweight untrusted WASM module to transform an integer. Provide either 'base64_wasm' (compiled WASM) or 'wat' (WebAssembly Text), and an 'input' integer. The WASM must export a function named 'transform' that takes one i32 and returns one i32. Execution is fuel-limited.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "base64_wasm": {
                        "type": "string",
                        "description": "Base64 encoded compiled WASM binary (optional if wat is provided)"
                    },
                    "wat": {
                        "type": "string",
                        "description": "WebAssembly Text format (optional if base64_wasm is provided)"
                    },
                    "input": {
                        "type": "integer",
                        "description": "The i32 input to the transform function"
                    }
                },
                "required": ["input"]
            })
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let wasm_bytes = if let Some(wat_str) = args.wat {
            wat::parse_str(wat_str)?
        } else if let Some(b64) = args.base64_wasm {
            general_purpose::STANDARD.decode(b64)?
        } else {
            return Err(WasmTransformerError::Execution("Must provide either base64_wasm or wat".to_string()));
        };

        let mut config = Config::default();
        config.consume_fuel(true);

        let engine = Engine::new(&config);
        let module = Module::new(&engine, &wasm_bytes[..])?;

        let linker = <Linker<wasmi::StoreLimits>>::new(&engine);
        let limits = wasmi::StoreLimitsBuilder::new().memory_size(10 * 65536).build(); // Limit memory to ~650KB (10 pages)
        let mut store = Store::new(&engine, limits);
        store.limiter(|limits| limits);

        store.set_fuel(self.fuel_limit).map_err(|e| WasmTransformerError::Execution(format!("Failed to set fuel: {:?}", e)))?;

        let instance = linker
            .instantiate(&mut store, &module)?
            .start(&mut store)?;

        let transform = instance
            .get_typed_func::<i32, i32>(&store, "transform")
            .map_err(|_| WasmTransformerError::MissingExport)?;

        let result = transform.call(&mut store, args.input)?;

        let remaining_fuel = store.get_fuel().unwrap_or(0);
        let fuel_consumed = self.fuel_limit.saturating_sub(remaining_fuel);

        Ok(WasmTransformerResult {
            output: result,
            fuel_consumed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_wasm_fuel_limit() {
        // Infinite loop in WAT format
        let wat_code = r#"
        (module
            (func $transform (param $p i32) (result i32)
                (loop $my_loop
                    br $my_loop
                )
                local.get $p
            )
            (export "transform" (func $transform))
        )
        "#;

        let tool = WasmTransformer::new(100);
        let args = WasmTransformerArgs {
            base64_wasm: None,
            wat: Some(wat_code.to_string()),
            input: 42,
        };

        let result = tool.call(args).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(
            err_str.contains("out of fuel") || err_str.contains("OutOfFuel") || err_str.contains("all fuel consumed by WebAssembly"),
            "Unexpected error: {}",
            err_str
        );
    }

    #[tokio::test]
    async fn test_wasm_success() {
        // Simple add 10 function
        let wat_code = r#"
        (module
            (func $transform (param $p i32) (result i32)
                local.get $p
                i32.const 10
                i32.add
            )
            (export "transform" (func $transform))
        )
        "#;

        let tool = WasmTransformer::new(1000);
        let args = WasmTransformerArgs {
            base64_wasm: None,
            wat: Some(wat_code.to_string()),
            input: 32,
        };

        let result = tool.call(args).await.expect("Failed to execute valid wasm");
        assert_eq!(result.output, 42);
        assert!(result.fuel_consumed > 0);
    }

    #[tokio::test]
    async fn test_wasm_memory_limit() {
        let wat_code = r#"
        (module
            (memory 11)
            (func $transform (param $p i32) (result i32)
                local.get $p
            )
            (export "transform" (func $transform))
        )
        "#;

        let tool = WasmTransformer::new(1000);
        let args = WasmTransformerArgs {
            base64_wasm: None,
            wat: Some(wat_code.to_string()),
            input: 42,
        };

        let result = tool.call(args).await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        let err_str = format!("{}", err);
        assert!(
            err_str.contains("out of bounds memory allocation"),
            "Unexpected error: {}",
            err_str
        );
    }
}
