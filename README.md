# AdsConsole

App desktop que liga o seu agente de IA (Claude Code, Claude Desktop, Codex, Gemini CLI, Cursor) às suas contas de Google Ads e Meta Ads. A IA lê contas, campanhas e métricas e propõe mudanças. Cada escrita passa pela sua aprovação no app.

- **Local:** não existe servidor do AdsConsole. Credenciais ficam no keychain do sistema e dados num banco SQLite local.
- **Suas credenciais:** você usa o seu próprio app da Meta e o seu próprio projeto do Google Cloud.
- **Escritas controladas:** cada plataforma tem um modo de escrita (só leitura, aprovação ou direta). O padrão é aprovação, com histórico e desfazer.

## Instalar

Baixe o instalador da [última release](../../releases/latest):

- **macOS:** `.dmg` (Apple Silicon ou Intel). O app ainda não é assinado: na primeira vez, clique com o botão direito no AdsConsole e escolha **Abrir**.
- **Windows:** `.msi` ou `.exe`. Em "O Windows protegeu o computador", clique em **Mais informações → Executar assim mesmo**.
- **Linux:** `.AppImage` (dê permissão de execução) ou `.deb`.

O app avisa quando sair uma versão nova.

## Primeiros passos

1. **Conectar uma plataforma.** Em **Conexões**, o assistente do Google pede o JSON do cliente OAuth (tipo **App para computador**, num projeto com a Google Ads API ativada). O da Meta pede App ID, App Secret e o `config_id` do Login do Facebook para Empresas. Os dois têm links diretos para cada tela de configuração.
2. **Conectar o cliente de IA.** Em **MCPs**, escolha o cliente e copie o trecho pronto (o comando ou o JSON já levam o endereço e o token).
3. **Usar.** Peça algo à IA, como "liste minhas campanhas". Quando ela pedir uma mudança, o sistema avisa e você aprova ou recusa em **Aprovações**.

O AdsConsole fica na barra de menu (ou bandeja) com o número de aprovações pendentes. Fechar a janela não para os MCPs.

## Páginas que você precisa publicar

A Meta e o Google pedem URLs públicas para o seu app. Os modelos estão em [`docs/templates/`](docs/templates/):

- [`privacy-policy.md`](docs/templates/privacy-policy.md): política de privacidade. Preencha os campos `{{ }}`, publique em qualquer site seu e cole a URL na Meta (URL da Política de Privacidade e instruções de exclusão de dados) e no Google Cloud (Branding da tela de consentimento).
- [`meta-oauth-relay.html`](docs/templates/meta-oauth-relay.html): página de retorno do login da Meta, necessária com o app publicado (modo Ativo), porque a Meta não aceita `localhost` nesse modo. Publique num endereço `https`, cadastre-o em "URIs de redirecionamento do OAuth válidos" do app e informe no assistente da Meta.

## Configuração avançada

A pasta de dados (`~/Library/Application Support/adsconsole` no macOS) guarda o `config.json`, o banco local e os logs. O app edita tudo pelas telas, mas variáveis de ambiente têm prioridade: `ADSCONSOLE_DATA_DIR`, `ADSCONSOLE_PORT`, `ADSCONSOLE_TOKEN`.

## Desenvolvimento

Requisitos: Rust 1.95 (o `rust-toolchain.toml` instala a versão certa via `rustup`) e Node 22. No Linux, as dependências do Tauri: `libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev libxdo-dev patchelf`.

```sh
npm --prefix ui ci
cd crates/app && npx --prefix ../../ui tauri dev               # app com recarga
cd crates/app && npx --prefix ../../ui tauri dev -- -- --demo  # dados de exemplo, nada sai do computador
```

Testes:

```sh
cargo test --workspace
npm --prefix ui test
npm --prefix ui run build && npm --prefix ui run e2e
```

Depois de mudar um comando em `crates/app`, regenere os tipos da UI com `npm --prefix ui run bindings`.

Exemplos de linha de comando (úteis para testar contra contas reais sem o app):

```sh
cargo run -p adsconsole-mcp --example google_connect -- caminho/client.json "Minha conta"
cargo run -p adsconsole-mcp --example meta_connect -- "Minha conta" --login <app_id> <config_id> [--relay <url>]
cargo run -p adsconsole-mcp --example google_server
cargo run -p adsconsole-mcp --example meta_server
```

Para publicar uma versão, siga o [`RELEASING.md`](RELEASING.md).

## Licença

[GNU AGPL-3.0](LICENSE). Você pode usar, modificar e redistribuir o AdsConsole, desde que mantenha os avisos de autoria e publique o código-fonte das suas versões sob a mesma licença, inclusive quando oferecê-las como serviço pela rede.
