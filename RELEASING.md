# Como publicar uma versão

1. Atualize a versão em `crates/app/Cargo.toml`, `crates/app/tauri.conf.json` e `ui/package.json`.
2. Rode os testes: `cargo test --workspace`, `npm --prefix ui test` e `npm --prefix ui run e2e`.
3. Crie a tag `vX.Y.Z` na `main` e envie (`git push origin vX.Y.Z`). O workflow **Release** gera os instaladores e cria um rascunho de release.
4. Faça o checklist abaixo com os instaladores do rascunho.
5. Publique o rascunho. A partir daí, o app instalado avisa da versão nova.

## Checklist manual (macOS, Windows e Linux)

- [ ] Instalar e abrir pela primeira vez (aviso do Gatekeeper no macOS, SmartScreen no Windows).
- [ ] Conectar Google Ads com um projeto real (assistente, JSON do cliente, login no navegador).
- [ ] Conectar Meta Ads com um app real, por login e por token.
- [ ] Pedir uma escrita pela IA no modo aprovação: chega a notificação, o contador aparece na barra de menu, aprovar aplica.
- [ ] Desfazer a mudança pelo Histórico.
- [ ] Pausar e retomar um MCP pela barra de menu.
- [ ] Fechar a janela (os MCPs continuam) e reabrir pela barra de menu.
- [ ] Ligar "Abrir com o sistema", reiniciar o computador e conferir.
- [ ] Sair com uma aprovação pendente: o app pede confirmação.
- [ ] Trocar idioma e tema.
- [ ] Clientes de IA: Claude Code, Claude Desktop, Codex, Gemini CLI e Cursor conectam com o trecho copiado e listam as ferramentas.
