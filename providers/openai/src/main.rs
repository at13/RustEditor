fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "rusteditor-openai: RustEditor provider for OpenAI, Anthropic and GitHub Copilot\n\n\
             Reads a request on stdin and writes JSON-line events on stdout.\n\n\
             Options:\n  --backend NAME          openai (default), anthropic, copilot\n  \
             --credential ID         read openai/anthropic API key from the OS credential store\n  \
             --copilot-sdk PATH      Copilot SDK provider entrypoint (default copilot-sdk/dist/main.js beside helper)\n  \
             --check-auth            check SDK authentication without a document/model request\n  \
             --base-url URL          default https://api.openai.com/v1 (Ollama: http://localhost:11434/v1)\n  \
             --api-key-env NAME      environment variable holding the key (default OPENAI_API_KEY)\n  \
             --auth-header NAME      header for the key (default Authorization: Bearer; Azure: api-key)\n  \
             --model NAME            overrides the model in the request\n  \
             --max-tokens-field NAME default max_tokens (some models need max_completion_tokens)\n  \
             --no-usage              do not request token usage (for servers that reject stream_options)"
        );
        return;
    }
    let code = rusteditor_openai::run(&args, std::io::stdin().lock(), std::io::stdout().lock());
    std::process::exit(code);
}
