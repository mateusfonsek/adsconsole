<!--
Template da política de privacidade do AdsConsole.

A Meta e o Google exigem uma URL pública de política de privacidade para o app que você
cria para usar o AdsConsole. Copie este arquivo, troque os campos entre {{ }} e publique
em qualquer site seu (GitHub Pages, Cloudflare Pages, Netlify...). Depois cole a URL em:
- Meta: Configurações do app → Básico → URL da Política de Privacidade
  (e, em "Exclusão de dados do usuário", a mesma URL com #apagar-dados).
- Google Cloud: Google Auth Platform → Branding → Link da Política de Privacidade.

Campos:
- {{RESPONSAVEL}}: seu nome ou o da sua empresa.
- {{EMAIL}}: e-mail de contato para pedidos sobre dados.
- {{URL}}: o endereço onde esta página vai ficar publicada.
- {{DATA}}: data da última atualização.

Apague este comentário antes de publicar.
-->

# Política de privacidade do AdsConsole

*Última atualização: {{DATA}}*

Esta política vale para o app que {{RESPONSAVEL}} usa com o AdsConsole, um software de código aberto que roda **inteiramente no computador de quem o instala**. Não existe servidor do AdsConsole: nenhum dado de quem usa é recebido, armazenado ou visto por terceiros.

## Quem é o responsável pelos dados

{{RESPONSAVEL}} instala o AdsConsole e conecta **o próprio app** da Meta e **o próprio projeto** do Google Cloud. Os dados ficam sob o controle de {{RESPONSAVEL}}, no computador onde o AdsConsole roda.

## Que dados o app usa

- **Credenciais:** App ID, App Secret e tokens de acesso da Meta; client id, client secret e refresh token do Google. Ficam no keychain do sistema operacional (ou, sem keychain, num arquivo legível só pelo usuário) e nunca saem do computador, exceto quando enviados às próprias APIs da Meta e do Google para autenticar.
- **Dados de anúncios:** contas, campanhas, públicos, criativos e métricas, lidos e alterados a pedido do usuário. Ficam em um banco de dados local (histórico de mudanças e cache de contas) e em logs locais, com os segredos mascarados.

## Com quem os dados são compartilhados

Com ninguém além das próprias plataformas. O app conversa só com:

- a Graph API e o login da Meta (`graph.facebook.com`, `www.facebook.com`);
- a Google Ads API e o login do Google (`googleads.googleapis.com`, `oauth2.googleapis.com`, `accounts.google.com`).

Não há telemetria, analytics, anúncios nem venda de dados.

<a id="apagar-dados"></a>

## Como apagar os dados

- **No app:** remover a conexão apaga as credenciais dela do keychain.
- **Tudo:** apagar a pasta de dados do AdsConsole (ou a definida em `ADSCONSOLE_DATA_DIR`) remove o banco local e os logs.
- **Na Meta:** em Configurações do Facebook → Segurança e login → Apps e sites (ou Integrações comerciais), remover o app revoga o acesso.
- **No Google:** em https://myaccount.google.com/permissions, remover o acesso do app.

## Contato

Dúvidas e pedidos: {{EMAIL}}. Versão publicada: {{URL}}

---

# AdsConsole privacy policy

*Last updated: {{DATA}}*

This policy covers the app that {{RESPONSAVEL}} uses with AdsConsole, open source software that runs **entirely on the computer of whoever installs it**. There is no AdsConsole server: no user data is received, stored or seen by third parties.

**Who controls the data.** {{RESPONSAVEL}} installs AdsConsole and connects **their own** Meta app and **their own** Google Cloud project. The data stays on the computer where AdsConsole runs, under {{RESPONSAVEL}}'s control.

**What data the app uses.** Credentials (Meta App ID, App Secret and access tokens; Google client id, client secret and refresh token) are kept in the operating system keychain, or in a file readable only by the user when no keychain is available. They leave the computer only to authenticate with the Meta and Google APIs. Ad data (accounts, campaigns, audiences, creatives and metrics) is read and changed at the user's request and kept in a local database and local logs, with secrets masked.

**Sharing.** None beyond the platforms themselves: the app only talks to the Meta Graph API and login, and to the Google Ads API and login. No telemetry, analytics, ads or data sales.

**Deleting data.** Removing a connection in the app deletes its credentials. Deleting the AdsConsole data folder removes the local database and logs. Removing the app in your Facebook or Google account settings revokes its access.

**Contact.** {{EMAIL}}. Published version: {{URL}}
