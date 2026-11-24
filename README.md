# MLDO — Multi Link Download Organizer

Aplicativo desktop nativo em **Rust**, com interface em português, downloads concorrentes e organização automática após o download.

**v0.7.0** — O captcha só abre depois de iniciar a fila, quando um arquivo com vaga reservada precisa de acesso. Com vários downloads simultâneos, o MLDO apresenta uma página por vez.

## Como usar

1. Abra `dist\MLDO.exe`.
2. Selecione a lista de links `.txt` e `download_schema.json`.
3. Escolha **uma pasta-base**, que define o disco dos downloads, e de **1 a 5** transferências simultâneas.
4. Opcionalmente, marque **Conexões por arquivo** e selecione de **1 a 5**. Desativado, usa uma conexão; essa opção é independente do número de arquivos simultâneos.
5. Clique em **Iniciar downloads**. Nomes ausentes/repetidos e contagens diferentes não bloqueiam a fila.
6. Acompanhe os downloads **no centro**: nome, velocidade, bytes, percentual e segmentos P1–P5. Eventos, avisos e log exportável ficam **à direita**. Falhas/cancelamentos podem ser tentados novamente sem repetir os concluídos nesta sessão.

### Interface de progresso

- A coluna central é exclusiva para transferências, sem slogan ou log abaixo. Mostra de **1 a 5 arquivos ativos**, priorizados acima de concluídos/na fila, com filtro para listas maiores.
- Cada barra tem exatamente a quantidade de partes **realmente usada pelo arquivo**, não apenas o número configurado. Cada segmento P1–P5 possui seu próprio preenchimento e percentual; passe o mouse para ver os bytes daquela parte.
- Com uma conexão, a barra tem um segmento. Se o servidor exigir fallback, a barra é reiniciada com um segmento, sem manter progresso das partes descartadas. Tentativas novas também zeram a amostra anterior.
- Quando não houver tamanho informado, mostra bytes recebidos e `?` no segmento, sem inventar percentual. Durante a mesclagem, os segmentos recebidos ficam completos e o status informa a verificação; o arquivo só é concluído após organização/publicação.
- Esquerda e direita têm a mesma largura; o centro aproveita o restante da janela. O log à direita tem rolagem independente, quebra de linhas, **Exportar** e **Limpar**.
- O botão **Navegador** da coluna esquerda abre o gerenciamento da autenticação numa janela, sem substituir os cartões centrais.

Exemplo de resultado, escolhendo `D:\Downloads`:

```text
D:\Downloads\
  .mldo-pendentes\           ← temporários no mesmo disco
  Uploadteste\              ← pasta principal do schema
    audio\audio.flac
    video\...
  NotIdentify\              ← fora da pasta principal
    arquivo-sem-correspondencia.bin
```

Cada transferência é recebida em streaming em `.mldo-pendentes`, sincronizada e, **somente após EOF**, submetida ao comando interno Rust de identificação/movimentação. A publicação usa movimentação sem sobrescrita no mesmo volume, não uma cópia pelo `%TEMP%` de outro disco. Não altere a árvore de destino durante a execução.

### Múltiplas conexões e integridade

- A opção começa **desativada** e só pode ser alterada antes de iniciar a fila. Com **1**, não faz teste Range nem divide o arquivo.
- Com **2–5**, testa `Range: bytes=0-0`. Só divide quando recebe HTTP **206**, `Content-Range` válido com tamanho total conhecido e **ETag forte**. Servidores sem suporte, sem ETag ou apenas com ETag fraco usam uma conexão; não basta anunciar `Accept-Ranges`.
- Cada parte cobre um intervalo exclusivo de bytes e envia `If-Match`/`If-Range`. Exige o mesmo ETag, URL efetiva, total e intervalo, sem compressão, e verifica o número de bytes recebido. Arquivos minúsculos usam menos partes quando necessário.
- Se o servidor ignorar Range ou mudar a representação, descarta **todas** as partes dessa tentativa e baixa uma cópia nova com uma conexão. Não junta partes de versões diferentes. Falhas de transporte/autenticação/limitação HTTP seguem o tratamento normal, não um contorno de restrições.
- Depois exibe **Mesclando e verificando**: junta na ordem em streaming, relê e compara SHA-256 de cada parte com o calculado na recepção, confere o tamanho total e sincroniza o arquivo antes de organizar. Falha na verificação não publica o resultado.
- Partes e arquivo mesclado ficam na **mesma pasta temporária/disco**. A mesclagem pode precisar de aproximadamente **duas vezes o tamanho do arquivo** em espaço temporário. Cancelamento normal também limpa partes e mesclagem incompletas.
- Progresso e velocidade são a soma das partes por arquivo, sem contar duas vezes a mesclagem ou o byte do teste. **5 arquivos × 5 partes = até 25 transferências HTTP de partes**; o teste de cada arquivo vem antes de suas partes. HTTP/2 pode multiplexar requisições em uma conexão física.
- Mais conexões **não garantem maior velocidade**: depende do servidor, limite de conta, rede e disco. Não há retomada persistente de partes. URLs de uso único ou servidores que recusam múltiplas requisições podem exigir desativar a opção.
- ETag e hashes locais evitam misturar versões e detectam alteração local na mesclagem; **não comprovam autenticidade do conteúdo remoto**, nem substituem um hash confiável fornecido pelo publicador. Nenhum software pode prometer ausência absoluta de corrupção em qualquer servidor/hardware.

### Identificação automática

- Usa o nome de `Content-Disposition`, da API/navegador e, como indicação adicional, o fragmento `#nome`/`#caminho` da lista. Caminhos arbitrários de URLs não são nomes confiáveis.
- Compara o **tamanho final efetivamente recebido** ao schema: no JSON, a comparação é exata, sem arredondamento de KB.
- Sem nome reconhecível, um tamanho único no schema pode identificar o arquivo. Sem tamanho no formato antigo, é necessário nome/caminho compatível.
- Nome repetido pode ser desambiguado pelo tamanho final ou caminho completo indicado. Não usa ordem dos links nem data de criação local.
- Nenhuma correspondência, ambiguidade, tamanho incompatível ou destino ocupado → **`pasta-base\NotIdentify`**, automaticamente. Colisões nessa pasta recebem sufixos como `arquivo (1).bin`.
- Um candidato não é escolhido apenas porque os demais já existem. Arquivos existentes nunca são substituídos.
- Nome/tamanho não comprovam autenticidade: o schema não contém hashes de conteúdo. Nada baixado é executado ou extraído.

Erros de transporte, corpo truncado e páginas HTML inesperadas continuam sendo falhas de download, não arquivos completos para organização. O servidor pode exigir autenticação antes de fornecer o arquivo.

### Completos preservados e comando sem rede

Um completo recebe um registro `.download.json` antes da tentativa de movimento. Se **também** houver falha de disco/permissão ao mover para `NotIdentify`, ele permanece em `.mldo-pendentes` e aparece como **Baixado · falha ao mover**. Corrija a causa e use **Reprocessar completos (sem rede)**.

Ao iniciar novamente com a mesma lista/schema/base, registros compatíveis são recuperados automaticamente, sem repetir o download correspondente. Também há um comando local opcional:

```powershell
.\dist\MLDO.exe --organize --schema ".\Prompt\download_schema.json" --destination "D:\Downloads"
```

Ele usa a mesma rotina do pós-download, sem GUI, shell ou rede, e gera `mldo-organize-*.log` na base. Somente move completos com registro válido e tamanho inalterado; não percorre arquivos arbitrários ou temporários parciais. Código de saída 0 indica sucesso; 1 indica erro. Por ser executável Windows gráfico, a saída de console pode não aparecer; consulte o relatório.

Registros contêm nome, bytes e hashes da origem/schema, **não URLs/cookies/tokens**. Trocar caminhos/tamanhos do schema pode impedir recuperação automática de registros antigos; preserve esses arquivos. Não há retomada por byte nem persistência completa da fila. Incompletos são removidos em falha/cancelamento normal; encerramento forçado pode deixar temporários sem registro. Não apague a área temporária com completos pendentes.

## Schema JSON

Array exportado pelo PowerShell, com tipos explícitos:

```json
[
  {"Tipo":"pasta","Nome":"MeuJogo","Caminho":".\\MeuJogo","Tamanho_Bytes":null,"Criado_Em":"2026-11-21 10:00"},
  {"Tipo":"pasta","Nome":"vazia","Caminho":".\\MeuJogo\\vazia","Tamanho_Bytes":null,"Criado_Em":"2026-11-21 10:00"},
  {"Tipo":"arquivo","Nome":"arquivo.bin","Caminho":".\\MeuJogo\\arquivo.bin","Tamanho_Bytes":13312,"Criado_Em":"2026-11-21 10:00"}
]
```

- `Tipo`: `pasta` ou `arquivo`. `Nome` deve corresponder ao último componente de `Caminho`.
- `Tamanho_Bytes`: `null` para pastas; inteiro não negativo para arquivos, incluindo zero. Preserva pastas vazias e arquivos vazios.
- `Criado_Em`: `yyyy-MM-dd HH:mm` ou com segundos. É contexto local, não prova de identidade; não modifica datas dos downloads.
- Aceita caminhos relativos com/sem `.\`. Caminhos absolutos Windows de origem são normalizados apenas se todos pertencerem a uma única pasta principal explicitamente declarada.
- O anexo `C:\Games\GTA6\GTA6.Xbox.v1.0.2.1-CYBERLEEK\...` vira `pasta-base\GTA6.Xbox.v1.0.2.1-CYBERLEEK\...`. **Não escreve no caminho original do schema.**
- Rejeita traversal (`..`), UNC, streams NTFS, nomes reservados, caminhos repetidos/conflitantes e mistura de caminhos absolutos/relativos. `.mldo-pendentes` e `NotIdentify` são nomes reservados na raiz da base.

O anexo `comandoschema.txt` é um **exportador**, não um script de movimentação. O MLDO lê somente os dados; não executa comandos do JSON ou desse anexo.

### Conjuntos fornecidos

- `Prompt\download_schema.json` + `Prompt\links.txt`: **108 arquivos, 42 pastas e 108 links**. `Update.alignmentchunk` tem exatamente **1 byte**.
- `C:\Projetos\UploadTeste\download_schema.json` + `C:\Projetos\links1.txt`: **11 arquivos, 6 pastas e 11 links sem nome**. Resultado sob `pasta-base\Uploadteste`.
- Anexos originais não são alterados. Use a lista correspondente a cada schema. Contagem divergente é informativa: não inventa correspondências nem bloqueia downloads.

### Formatos antigos

Continua aceitando TXT com TAB/espaços, cabeçalho opcional, tamanho `KB`/`KiB` opcional, ponto/vírgula decimal e datas `dd/MM/aaaa HH:mm[:ss]`. KB antigo significa KiB (1024 bytes), com tolerância de arredondamento de aproximadamente ±5 bytes; isso **não** é aplicado ao JSON.

Tabelas antigas sem tipo inferem pastas pela ancestralidade, não pelo tamanho zero, e não distinguem pastas vazias de arquivos sem filhos. Também são aceitas árvores Windows `tree /F /A` (ASCII/Unicode) e listas relativas, com barra final para pastas vazias:

```text
Jogo/arquivo.bin
Jogo/dados/parte02.bin
Jogo/pasta-vazia/
```

Codificações de entrada: UTF-8 com/sem BOM, UTF-16 com BOM e Windows-1252. Limite de 8 MiB por arquivo.

## Links, sites e autenticação

Um endereço HTTP/HTTPS por linha; comentários `#`/`//` e linhas vazias são ignorados. Somente o total é mostrado na importação, sem despejar URLs. Fragmentos são opcionais e não são enviados ao site:

```text
https://vikingfile.com/f/ID_A
https://gofile.io/d/ID_B#arquivo.bin
https://buzzheavier.com/ID_C#Jogo/dados/parte02.bin
```

IDs ilustrativos. Sites suportados: **VikingFile, Gofile e Buzzheavier**, incluindo subdomínios válidos. Cada linha deve fornecer um único arquivo; não expande automaticamente pastas remotas. Gofile escolhe um único arquivo pelo nome indicado ou aceita pasta com somente um arquivo. APIs/protocolos de website podem mudar.

- Ao exigir captcha/login, abre **uma página de autenticação por vez**, em janela auxiliar trazida à frente e vinculada ao MLDO. As demais aguardam captura/fechamento da página atual.
- Resolva captcha/login manualmente e clique no Download do site. A WebView captura a URL autorizada, referer, UA nativo e cookies aplicáveis. O download nativo é cancelado para manter todas as gravações na fila do MLDO.
- Ao capturar o acesso, o MLDO continua o mesmo download automaticamente. Com duas vagas ocupadas, o terceiro arquivo aguarda a liberação de uma delas. **Pular arquivo** libera a vaga. Se o site pedir acesso novamente para o mesmo arquivo, use **Navegador** para reabrir a página.
- **Acessos / URLs diretas opcionais** permite renovar uma sessão ou fornecer URL autorizada, inclusive de CDN. Não contém seletores de pasta.
- Usa **User-Agent nativo Edge/WebView2**, sem Safari/Macintosh forçado. HTTP de sessão reutiliza o UA real; sem sessão usa identificação Edge de fallback. `Safari/537.36` no UA nativo é compatibilidade normal.
- **Cloudflare pode recusar WebViews ou HTTP mesmo depois do captcha.** Não há solver, alteração de fingerprint nem promessa de contorno de proteções.
- Fechar a janela auxiliar a minimiza. Perfil WebView2 isolado e temporário; pode gravar cache/cookies no disco e deixar restos em encerramento forçado. Não importa credenciais do navegador padrão.
- Pop-ups, protocolos locais e permissões sensíveis são bloqueados. Páginas remotas não recebem comandos privilegiados da barra. Comunicação privada por pipes, sem servidor HTTP de controle.
- Políticas do Windows podem limitar foco global; owner mantém a janela auxiliar acima do MLDO, sem topmost permanente.

Navegador requer **Microsoft Edge WebView2 Runtime x64**. HTTP/organização não dependem dele. Gofile admite `MLDO_GOFILE_TOKEN` e `MLDO_GOFILE_WEBSITE_TOKEN` opcionais, lidos ao iniciar a fila e não registrados no log.

## Segurança e limites

- Valida base real e rejeita links simbólicos/junções nos destinos. Não é sandbox contra outro processo local que troque diretórios durante a escrita.
- Publicação sem sobrescrita, inclusive contra arquivo criado durante a transferência. Temporários e fallback ficam sob a mesma base.
- Fechar o MLDO cancela a fila e aguarda limpeza normal. Falhas temporárias HTTP 408/429/5xx têm até três tentativas; outros arquivos continuam.
- HTML/JSON inesperados não são tratados como binários. JSON é permitido quando o nome esperado/recebido tem extensão `.json`.
- URLs diretas e configuração ficam na sessão. Log limitado a 1.500 eventos, sem URLs assinadas/cookies; progresso amostrado aproximadamente a cada 350 ms.
- Testes usam HTTP local e corpos sintéticos. **Não comprovam disponibilidade dos links reais ou resolução de captchas remotos.** Nenhum arquivo real dos jogos foi baixado.

## Compilar e testar

Rust estável e ferramentas C++ MSVC do Visual Studio no Windows:

```powershell
cargo run --locked
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
powershell -ExecutionPolicy Bypass -File scripts\build.ps1
```

Neste ambiente há toolchain isolada em `%LOCALAPPDATA%\Temp\opencode\mldo-rust`, sem alterar PATH global:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\rust-env.ps1 test --locked
```

`scripts\rust-env.ps1 -Install` instala essa toolchain opcional, não as ferramentas C++. O executável final não depende do Cargo; pode requerer Microsoft Visual C++ Redistributable x64 em outra máquina.

Código: `src/schema.rs` (JSON/TXT), `src/mapping.rs` (fila sem associações obrigatórias), `src/download.rs` (rede/progresso), `src/segmented.rs` (intervalos e mesclagem), `src/staging.rs` (identificar/mover), `src/organizer_cli.rs` (comando local), `src/safety.rs` (destinos), `src/ui.rs` (interface), `src/hosts.rs` (sites) e `src/browser*.rs`/`src/window_focus.rs` (autenticação). Histórico: `Prompt\memoria\MLDO.md`.

Use somente arquivos cujo download você tem autorização para realizar. Referências open source e licenças: `THIRD_PARTY.md`.
