import tiktoken
import urllib.request
import urllib.error
import json
import time

ENCODING = "cl100k_base"

# Same query, different prompt lengths
PROMPTS = {
    "minimal": "Identify the language and count lines in this code:\n```\nfn main() {}\n```",
    "standard": "You are a code analysis assistant. Identify the programming language and count the lines.\n\nHere is the code:\n```\nfn main() {}\n```",
    "full": "You are a code analysis assistant. Your task is to analyze the provided code snippet and report:\n1. The programming language\n2. The number of lines of code\n3. A brief summary of what the code does\n\nHere is the code:\n```\nfn main() {\n    println!(\"hello from GPU world\");\n    let x = 42;\n    for i in 0..10 {\n        println!(\"iteration {}\", i);\n    }\n}\n```",
    "verbose": "You are a code analysis assistant. Your task is to analyze the provided code snippet and report:\n1. The programming language\n2. The number of lines of code\n3. A brief summary of what the code does\n4. The key functions/statements\n5. Any potential issues or improvements\n\nHere is the code:\n```\nfn main() {\n    println!(\"hello from GPU world\");\n    let x = 42;\n    for i in 0..10 {\n        println!(\"iteration {}\", i);\n    }\n}\n```\nPlease be thorough in your analysis."
}

def count_tokens(text):
    enc = tiktoken.get_encoding(ENCODING)
    return len(enc.encode(text))

def test_nvidia(prompt_name, prompt_text, api_key):
    url = "https://integrate.api.nvidia.com/v1/chat/completions"
    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
    }
    payload = {
        "model": "nvidia/nemotron-3.5-lightning-30b-a3b",
        "messages": [{"role": "user", "content": prompt_text}],
        "temperature": 0.0,
        "max_tokens": 256,
        "stream": False,
    }
    req = urllib.request.Request(url, data=json.dumps(payload).encode(), headers=headers, method="POST")
    
    start = time.time()
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            body = json.loads(resp.read().decode())
            elapsed = time.time() - start
            answer = body["choices"][0]["message"]["content"]
            usage = body.get("usage", {})
            return {
                "prompt_tokens": usage.get("prompt_tokens", count_tokens(prompt_text)),
                "completion_tokens": usage.get("completion_tokens", len(tiktoken.get_encoding(ENCODING).encode(answer))),
                "total_tokens": usage.get("total_tokens", count_tokens(prompt_text) + len(tiktoken.get_encoding(ENCODING).encode(answer))),
                "latency_ms": elapsed * 1000,
                "answer": answer[:200],
            }
    except urllib.error.HTTPError as e:
        return {"error": f"HTTP {e.code}: {e.reason}"}

if __name__ == "__main__":
    import os
    api_key = os.environ.get("NVIDIA_API_KEY", "")
    if not api_key:
        print("NVIDIA_API_KEY not set")
        exit(1)
    
    print("=== Prompt Length vs Latency/Quality Test ===\n")
    print(f"{'Prompt':<12} {'Tokens':>8} {'Latency':>10} {'Output':>8} {'Total':>8}")
    print("-" * 60)
    
    for name, prompt in PROMPTS.items():
        tokens = count_tokens(prompt)
        result = test_nvidia(name, prompt, api_key)
        if "error" in result:
            print(f"{name:<12} {tokens:>8} {'ERROR':>10} {'-':>8} {'-':>8}")
        else:
            print(f"{name:<12} {result['prompt_tokens']:>8} {result['latency_ms']:>10.0f}ms {result['completion_tokens']:>8} {result['total_tokens']:>8}")
            print(f"  Answer: {result['answer'][:100]}...")
        time.sleep(1)  # Rate limit
