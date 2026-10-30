# AdsConsole

Servidores MCP para Google Ads e Meta Ads que rodam no seu computador. Um agente de IA (Claude Code, Claude Desktop ou outro cliente MCP) lê contas, campanhas e métricas e propõe mudanças. Cada escrita passa pela sua aprovação.

- **Local:** não existe servidor do AdsConsole. Credenciais ficam no keychain do sistema e dados num banco SQLite local.
- **Suas credenciais:** você usa o seu próprio app da Meta e o seu próprio projeto do Google Cloud.
- **Escritas controladas:** cada plataforma tem um modo de escrita (`read_only`, `approval` ou `direct`). O padrão é `approval`.

## Requisitos

- Rust 1.95 (o `rust-toolchain.toml` instala a versão certa via `rustup`).
- Um projeto no Google Cloud com a Google Ads API ativada e um cliente OAuth do tipo **App para computador** (baixe o JSON do cliente).
- Um app na Meta for Developers com o caso de uso **Login do Facebook para Empresas** (você vai precisar do App ID, do App Secret e do `config_id` da configuração de login).

## Conectar as contas

As conexões ficam na pasta de dados do AdsConsole (`ADSCONSOLE_DATA_DIR` ou a pasta padrão do sistema, por exemplo `~/Library/Application Support/adsconsole` no macOS).

**Google Ads**

```sh
cargo run -p adsconsole-mcp --example google_connect -- caminho/client.json "Minha conta" \
    [--login-customer-id N] [--access-level test|explorer|basic|standard]
```

Abre o login do Google no navegador e lista as contas acessíveis.

**Meta Ads**

```sh
cargo run -p adsconsole-mcp --example meta_connect -- "Minha conta" --login <app_id> <config_id> [--relay <url>]
```

O App Secret é lido de `META_APP_SECRET` ou digitado no terminal, nunca passado como argumento. Também é possível conectar com um token de acesso já existente: `--token [--app-id <id>]`, com o token em `META_ACCESS_TOKEN` ou digitado.

Com o app da Meta publicado (modo Ativo), a Meta não aceita `localhost` como retorno do login. Nesse caso, publique a página de retorno (veja [Páginas que você precisa publicar](#páginas-que-você-precisa-publicar)) e passe o endereço em `--relay`.

## Rodar os servidores

```sh
cargo run -p adsconsole-mcp --example google_server
cargo run -p adsconsole-mcp --example meta_server
```

Cada servidor imprime o endereço (`http://127.0.0.1:7458/mcp/google-ads` ou `/mcp/meta-ads`) e o token de acesso. A porta padrão é 7458; para rodar os dois ao mesmo tempo, dê outra porta a um deles com `ADSCONSOLE_PORT`.

No modo `approval`, os pedidos de escrita aparecem no terminal do servidor: `pendentes`, `aprovar <id>` ou `recusar <id>`.

Para usar no Claude Code:

```sh
claude mcp add --transport http adsconsole-google http://127.0.0.1:7458/mcp/google-ads \
    --header "Authorization: Bearer <token>"
```

## Configuração

`config.json` na pasta de dados (opcional):

```json
{
  "servers": {
    "google_ads": { "write_mode": "approval" },
    "meta_ads": { "write_mode": "read_only" }
  }
}
```

Variáveis de ambiente: `ADSCONSOLE_DATA_DIR`, `ADSCONSOLE_PORT`, `ADSCONSOLE_TOKEN`.

## Páginas que você precisa publicar

A Meta e o Google pedem URLs públicas para o seu app. Os modelos estão em [`docs/templates/`](docs/templates/):

- [`privacy-policy.md`](docs/templates/privacy-policy.md): política de privacidade. Preencha os campos `{{ }}`, publique em qualquer site seu e cole a URL na Meta (URL da Política de Privacidade e instruções de exclusão de dados) e no Google Cloud (Branding da tela de consentimento).
- [`meta-oauth-relay.html`](docs/templates/meta-oauth-relay.html): página de retorno do login da Meta, necessária com o app publicado. Publique num endereço `https`, cadastre-o em "URIs de redirecionamento do OAuth válidos" do app e use em `--relay`.

## Desenvolvimento

```sh
cargo test --workspace
```

## Licença

[GNU AGPL-3.0](LICENSE). Você pode usar, modificar e redistribuir o AdsConsole, desde que mantenha os avisos de autoria e publique o código-fonte das suas versões sob a mesma licença, inclusive quando oferecê-las como serviço pela rede.
