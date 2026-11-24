# Referências open source — integração do navegador

A integração usa as APIs públicas dos projetos abaixo, sem copiar um navegador completo ou código de um solucionador de captchas.

| Projeto | Repositório | Licença |
| --- | --- | --- |
| Wry 0.57 | https://github.com/tauri-apps/wry | MIT ou Apache-2.0 |
| Tao 0.37 | https://github.com/tauri-apps/tao | Apache-2.0 |
| webview2-com | https://github.com/wravery/webview2-rs | MIT |

Wry apresenta a WebView nativa e fornece os eventos de navegação/download e acesso aos cookies aplicáveis. Tao fornece a janela e o event loop do processo auxiliar. No Windows o motor é o **Microsoft Edge WebView2 Runtime**, produto da Microsoft distribuído separadamente, não um motor Safari.

Referências técnicas usadas:
- https://docs.rs/wry/0.57.3/wry/struct.WebViewBuilder.html
- https://docs.rs/wry/0.57.3/wry/struct.WebView.html
- https://github.com/tauri-apps/wry/tree/dev/examples
- https://github.com/tauri-apps/tao/tree/dev/examples

O código específico do MLDO (barra local, protocolo privado, associação de destinos e política de download) foi implementado para este projeto. Não há dependência de serviço de solver, automação de captcha ou importação de sessões de outros navegadores.

Outras dependências Rust e versões exatas estão em `Cargo.toml`/`Cargo.lock`; suas licenças permanecem com os respectivos autores. Esta lista destaca a integração nova e não é um inventário completo das dependências transitivas.
