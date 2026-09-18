import tiktoken
import json
import urllib.request
import urllib.error

ENCODING = "cl100k_base"

SAMPLE_CODE = '''fn main() {
    println!("hello from GPU world");
    let x = 42;
    for i in 0..10 {
        println!("iteration {}", i);
    }
}'''

SAMPLE_DIFF = '''--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
+    println!("GPU computing");
     println!("hello");
 }
 context line'''

FORMATS = {
    "plain_markdown": f"```rust\n{SAMPLE_CODE}\n```",
    "xml_only": f'<file path="src/main.rs">\n{SAMPLE_CODE}\n</file>',
    "sentinel_xml": f'<file path="src/main.rs">\n§src/main.rs\n¶\n{SAMPLE_CODE}\n</file>',
    "sentinel_only": f'§src/main.rs\n¶\n{SAMPLE_CODE}',
}


def count_tokens(text: str) -> int:
    enc = tiktoken.get_encoding(ENCODING)
    return len(enc.encode(text))


def count_tokens_bytes(data: bytes) -> int:
    enc = tiktoken.get_encoding(ENCODING)
    return len(enc.encode(data.decode("utf-8", errors="replace")))


def print_token_report():
    print("=== Token Cost Comparison (cl100k_base) ===\n")
    for name, content in FORMATS.items():
        tokens = count_tokens(content)
        print(f"{name}: {tokens} tokens, {len(content.encode('utf-8'))} bytes")
        print(f"  preview: {content[:80]!r}")
        print()

    # Savings vs xml_only
    xml_tokens = count_tokens(FORMATS["xml_only"])
    sent_xml_tokens = count_tokens(FORMATS["sentinel_xml"])
    sent_only_tokens = count_tokens(FORMATS["sentinel_only"])
    plain_tokens = count_tokens(FORMATS["plain_markdown"])

    print("--- Token savings vs XML ---")
    print(f"sentinel+XML vs XML:       {sent_xml_tokens}/{xml_tokens} = {sent_xml_tokens/xml_tokens*100:.1f}% ({xml_tokens - sent_xml_tokens} fewer tokens)")
    print(f"sentinel-only vs XML:      {sent_only_tokens}/{xml_tokens} = {sent_only_tokens/xml_tokens*100:.1f}% ({xml_tokens - sent_only_tokens} fewer tokens)")
    print(f"sentinel-only vs Markdown:  {sent_only_tokens}/{plain_tokens} = {sent_only_tokens/plain_tokens*100:.1f}% ({sent_only_tokens - plain_tokens} {'fewer' if sent_only_tokens < plain_tokens else 'more'} tokens)")
    print(f"sentinel+XML vs Markdown:   {sent_xml_tokens}/{plain_tokens} = {sent_xml_tokens/plain_tokens*100:.1f}% ({sent_xml_tokens - plain_tokens} {'fewer' if sent_xml_tokens < plain_tokens else 'more'} tokens)")


def test_llm_nvidia(api_key: str, model: str = "nvidia/nemotron-3.5-lightning-30b-a3b"):
    """Send the 3 formats to NVIDIA and compare responses."""
    url = "https://integrate.api.nvidia.com/v1/chat/completions"
    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
    }

    results = {}
    for name, content in FORMATS.items():
        payload = {
            "model": model,
            "messages": [
                {"role": "system", "content": "You are a code analysis assistant. Identify the programming language and count the lines."},
                {"role": "user", "content": f"Analyze this code:\n{content}"},
            ],
            "temperature": 0.0,
            "max_tokens": 256,
            "stream": False,
        }
        req = urllib.request.Request(
            url,
            data=json.dumps(payload).encode(),
            headers=headers,
            method="POST",
        )
        try:
            with urllib.request.urlopen(req, timeout=120) as resp:
                body = json.loads(resp.read().decode())
                answer = body["choices"][0]["message"]["content"]
                reasoning = body["choices"][0].get("message", {}).get("reasoning_content", "")
                results[name] = {"tokens": count_tokens(content), "answer": answer, "reasoning": reasoning[:120]}
                print(f"[{name}] {count_tokens(content)} tokens")
                print(f"   answer: {answer}")
                if reasoning:
                    print(f"   reasoning: {reasoning[:120]!r}")
        except urllib.error.HTTPError as e:
            error_body = e.read().decode(errors="replace")
            results[name] = {"tokens": count_tokens(content), "error": str(e), "body": error_body[:300]}
            print(f"[{name}] {count_tokens(content)} tokens → HTTP {e.code}: {e.reason}")
            print(f"   error body: {error_body[:300]!r}")

    return results


if __name__ == "__main__":
    import os
    print_token_report()
    print()
    api_key = os.environ.get("NVIDIA_API_KEY", "")
    if api_key:
        print("=== Testing with NVIDIA API ===")
        test_llm_nvidia(api_key)
    else:
        print("NVIDIA_API_KEY not set — skipping LLM test")
        print("Set it with: export NVIDIA_API_KEY='nvapi-...'")
