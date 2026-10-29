<span id="deployment-guide"></span>

# Instrukcja wdrożenia

<span id="local-development"></span>

## Lokalny rozwój

<span id="prerequisites"></span>

### Wymagania wstępne

- Zestaw narzędzi Rust (stable)
- clang i libclang-dev (dla RocksDB)
- Node.js (buduje dashboard, który serwer osadza podczas kompilacji)
- Klucz API OpenAI dla operacji LLM (ekstrakcja, konsolidacja, dream); bez klucza uruchom z `LOOMEM_ALLOW_KEYLESS=1`

<span id="build-and-run"></span>

### Budowanie i uruchamianie

``` bash
git clone <repository-url>
cd loomem

# Zbuduj (najpierw dashboard: serwer osadza go podczas kompilacji)
cp entities.toml.example entities.toml
(cd loomem-dashboard && npm ci && npm run build)
cargo build --release -p loomem-server

# Lokalny model embeddingów (wymagany przy domyślnym embedding_provider = "local")
./scripts/fetch-embedding-model.sh

# Ustaw zmienne środowiskowe
export OPENAI_API_KEY="sk-..."       # lub: export LOOMEM_ALLOW_KEYLESS=1
export LOOMEM_AUTH_TOKEN="your-secret-token"

# Uruchom
./target/release/loomem-server

# Sprawdź
curl http://localhost:3030/health
```

<span id="first-memory"></span>

### Pierwsze wspomnienie

``` bash
curl -X POST http://localhost:3030/v1/store \
  -H "Authorization: Bearer your-secret-token" \
  -H "Content-Type: application/json" \
  -d '{"content": "Hello, this is my first memory"}'
```

<span id="local-embeddings-no-openai-dependency"></span>

### Lokalne embeddingi (bez zależności od OpenAI)

W sekcji `[llm]` pliku `config.toml` ustaw `embedding_provider = "local"` — to wartość domyślna nowej instalacji. Używany jest runtime tract ONNX, napisany w czystym Ruście i bez natywnych zależności; nic nie opuszcza maszyny. Model pobiera się osobno (`./scripts/fetch-embedding-model.sh`); bez niego serwer odmawia uruchomienia (`refusing cloud fallback`), zamiast przejść na OpenAI.

Uwaga: konsolidacja, ekstrakcja i dream korzystają z OpenAI completions (`gpt-4.1-mini`). Bez `OPENAI_API_KEY` serwer w domyślnej konfiguracji odmawia uruchomienia, chyba że ustawiono `LOOMEM_ALLOW_KEYLESS=1`; w takim trybie te operacje kończą się błędem (`memory_ingest` zwraca błąd braku klucza). Embeddingi i wyszukiwanie pozostają lokalne w obu przypadkach.

------------------------------------------------------------------------

<span id="docker"></span>

## Docker

<span id="build"></span>

### Budowanie

``` bash
docker build -t loomem .
```

<span id="run"></span>

### Uruchamianie

``` bash
docker run -d \
  --name loomem \
  -p 3030:3030 \
  -v loomem-data:/data \
  -e OPENAI_API_KEY="sk-..." \
  -e LOOMEM_EMBEDDING_PROVIDER=openai \
  -e LOOMEM_EMBEDDING_DIM=1536 \
  -e LOOMEM_AUTH_TOKEN="your-secret-token" \
  -e LOOMEM_AT_REST_MASTER_KEY="base64-encoded-32-bytes" \
  loomem
```

Dockerfile automatycznie:

- Nasłuchuje na `0.0.0.0` (dostęp z zewnątrz kontenera)
- Ustawia katalog danych na `/data` (zamontuj tu wolumen)
- Wymaga `LOOMEM_AUTH_TOKEN` i `LOOMEM_AT_REST_MASTER_KEY` podczas startu — bez nich serwer odmawia uruchomienia (świadome wyłączenie ochrony: `LOOMEM_ALLOW_UNAUTH=1`, `LOOMEM_AT_REST_EXPECT_ENABLED=0`)
- Włącza limity żądań dla strumieni (`[rate_limit]`; żądania ponad limit otrzymują HTTP 429 z `Retry-After`)
- Jest dostarczany **bez** lokalnego modelu embeddingów — ustaw `LOOMEM_EMBEDDING_PROVIDER=openai` i `LOOMEM_EMBEDDING_DIM=1536` (jak wyżej), w przeciwnym razie serwer odmówi uruchomienia. Przechowywany tekst będzie wtedy wysyłany do API embeddingów OpenAI. Baza zbudowana już z lokalnych wektorów o 384 wymiarach wymaga po zmianie uruchomienia `loomem-server --reembed`.

<span id="docker-compose"></span>

### Docker Compose

``` yaml
version: '3.8'
services:
  loomem:
    build: .
    ports:
      - "3030:3030"
    volumes:
      - loomem-data:/data
    environment:
      - OPENAI_API_KEY=${OPENAI_API_KEY}
      - LOOMEM_EMBEDDING_PROVIDER=openai
      - LOOMEM_EMBEDDING_DIM=1536
      - LOOMEM_AUTH_TOKEN=${LOOMEM_AUTH_TOKEN}
      - LOOMEM_AT_REST_MASTER_KEY=${LOOMEM_AT_REST_MASTER_KEY}
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:3030/health"]
      interval: 30s
      timeout: 10s
      retries: 3

volumes:
  loomem-data:
```

------------------------------------------------------------------------

<span id="railway"></span>

## Railway

<span id="one-click-deploy"></span>

### Wdrożenie jednym kliknięciem

1.  Utwórz fork repozytorium
2.  Połącz je z Railway
3.  Utwórz nowy projekt → Deploy from GitHub repo
4.  Dodaj zmienne środowiskowe: - `OPENAI_API_KEY` — Twój klucz API OpenAI - `LOOMEM_EMBEDDING_PROVIDER=openai` i `LOOMEM_EMBEDDING_DIM=1536` — obraz nie zawiera lokalnego modelu embeddingów - `LOOMEM_AUTH_TOKEN` — token administratora (wygeneruj losowy ciąg) - `LOOMEM_AT_REST_MASTER_KEY` — 32-bajtowy klucz główny zakodowany w base64, wymagany przez obraz Docker; przechowuj osobną bezpieczną kopię do odzyskiwania danych
5.  Dodaj wolumen zamontowany w `/data` dla trwałego przechowywania danych
6.  Wdróż

<span id="configuration"></span>

### Konfiguracja

Jeśli wdrażasz w Railway, dodaj `railway.toml` w katalogu głównym repozytorium (nie jest dostarczany domyślnie):

``` toml
[build]
dockerfilePath = "Dockerfile"

[deploy]
healthcheckPath = "/health"
healthcheckTimeout = 30
restartPolicyType = "ON_FAILURE"
restartPolicyMaxRetries = 3
```

Zmienna środowiskowa `PORT` jest ustawiana automatycznie przez Railway i zastępuje wartość `server.port` w konfiguracji.

<span id="custom-domain"></span>

### Własna domena

Po wdrożeniu możesz dodać własną domenę w ustawieniach Railway. Dostosuj do niej zmienną `SERVER_ORIGIN` (wymaganą dla OAuth).

------------------------------------------------------------------------

<span id="container-entrypoint-and-startup-migrations"></span>

## Punkt wejścia kontenera i migracje podczas uruchamiania

Obraz kontenera uruchamia się przez `scripts/docker-entrypoint.sh` (działa na dowolnej platformie kontenerowej). Obsługuje opcjonalną jednorazową migrację danych przed startem serwera:

| Zmienna środowiskowa | Przeznaczenie |
|----|----|
| `LOOMEM_MIGRATE_GRAPH_STREAMS_ON_START` | Ustaw `1`, aby przed startem serwera uruchomić `loomem-migrate --migrate-graph-entity-streams` (ponownie oznacza encje i krawędzie grafu, których strumień różni się od strumienia fragmentów, do których się odwołują). Brak ustawienia pomija migrację. |
| `LOOMEM_MIGRATE_GRAPH_STREAMS_COMMIT` | Ustaw `1`, aby faktycznie zapisać zmiany. Domyślnie wykonywany jest test bez zapisu. |
| `LOOMEM_MIGRATE_GRAPH_STREAMS_MANIFEST_DIR` | Katalog zapisu manifestów migracji. Domyślnie `/data/graph-migration-plans`. |

Migracja jest idempotentna (prawidłowe już encje są pomijane) i przy zapisie zmian tworzy checkpoint przed pierwszym zapisem. Po pomyślnej migracji usuń ustawienie flagi `ON_START` i wdróż ponownie, aby kolejne uruchomienia przebiegały szybką ścieżką bez zmian.

Inne podkomendy `loomem-migrate` (uruchamiane ręcznie, nie przez punkt wejścia): `--validate-graph-entity-streams` i `--sample-embeddings`.

------------------------------------------------------------------------

<span id="production-checklist"></span>

## Lista kontrolna dla produkcji

<span id="security"></span>

### Bezpieczeństwo

- \[ \] Ustaw silny `LOOMEM_AUTH_TOKEN` (co najmniej 64 losowe znaki szesnastkowe) — bez niego serwer akceptuje każde żądanie (lokalny tryb bez uwierzytelniania)
- \[ \] Przenieś klucze API do zmiennych środowiskowych (nie do config.toml)
- \[ \] Sprawdź ustawienia danych osobowych — upewnij się, że `pii.enabled = true`
- \[ \] Rozważ włączenie szyfrowania danych w spoczynku (`LOOMEM_AT_REST_MASTER_KEY`) — zobacz [model bezpieczeństwa](SECURITY.md)
- \[ \] Skonfiguruj HTTPS (zarządzane platformy go zapewniają; Docker wymaga reverse proxy)
- \[ \] Za reverse proxy ustaw `LOOMEM_TRUST_FORWARDED_FOR=1`, aby limit żądań `/oauth/register` korzystał z adresu klienta dodawanego przez proxy do `X-Forwarded-For`, zamiast przypisywać wszystkich do limitu proxy. Nie ustawiaj tego, gdy Loomem jest dostępny bezpośrednio: nagłówek może wtedy kontrolować klient.

W razie wycieku tokenu uwierzytelniania zmień jego wartość w zmiennej środowiskowej, uruchom usługę ponownie i sprawdź żądaniem, czy stary token jest odrzucany. Rotacja klucza głównego nie jest obecnie obsługiwana: jego zmiana nie zabezpiecza ponownie istniejących kluczy danych i uniemożliwia odczyt zaszyfrowanych danych. Zobacz [przegląd bezpieczeństwa](SECURITY.md).

<span id="performance"></span>

### Wydajność

- \[ \] Zamontuj `/data` na szybkim dysku (SSD)
- \[ \] Ustaw `storage.rocksdb.write_buffer_size` odpowiednio do dostępnej pamięci RAM
- \[ \] Sprawdź `storage.tantivy.heap_size_mb` — więcej RAM oznacza szybsze indeksowanie
- \[ \] Przy dużym ruchu rozważ włączenie `search.cache`

<span id="reliability"></span>

### Niezawodność

- \[ \] Włącz `storage.intent_log.enabled = true` na potrzeby odzyskiwania po awarii
- \[ \] Ustaw `storage.intent_log.sync_on_write = true` dla trwałości danych
- \[ \] Skonfiguruj kopię zapasową: `worker.backup.enabled = true`
- \[ \] Ustaw odpowiednie `resource_guards` dla swojego środowiska
- \[ \] Monitoruj przez endpoint `GET /v1/status`

<span id="cost-control"></span>

### Kontrola kosztów

- \[ \] Ustaw `cost.daily_cap_usd`, aby ograniczyć niekontrolowane koszty LLM
- \[ \] Ustaw `cost.alert_threshold_usd` dla wczesnych ostrzeżeń
- \[ \] Sprawdź `dream.cost_cap_usd_per_run` dla każdej sesji dream
- \[ \] Monitoruj dzienne koszty w rodzinie kolumn kosztów RocksDB

------------------------------------------------------------------------

<span id="connecting-mcp-clients"></span>

## Łączenie klientów MCP

<span id="claudeai-web"></span>

### Claude.ai (web)

1.  Settings → Customize → Connectors
2.  Dodaj nowy konektor
3.  Wpisz adres: `https://your-loomem.app/mcp`
4.  Dokończ OAuth — po wyświetleniu prośby podaj klucz API

<span id="claude-desktop"></span>

### Claude Desktop

Edytuj `claude_desktop_config.json`:

``` json
{
  "mcpServers": {
    "loomem": {
      "url": "https://your-loomem.app/mcp",
      "headers": {
        "Authorization": "Bearer loom_your_api_key"
      }
    }
  }
}
```

Uruchom Claude Desktop ponownie.

> Powyższa forma `url` dotyczy **zdalnej instancji dostępnej przez HTTPS**. Jeśli Loomem działa **lokalnie** przez zwykły HTTP (`http://127.0.0.1:<port>`), aplikacja komputerowa nie może użyć samego adresu `http://localhost` — połącz go przez stdio za pomocą `"command": "npx", "args": ["-y", "mcp-remote", "http://127.0.0.1:3030/mcp", "--allow-http"]`. Zobacz [podręcznik użytkownika](https://github.com/vvooki-sys/loomem/blob/main/docs/pl/user-guide.md).

<span id="claude-code-cli-vs-code"></span>

### Claude Code (CLI / VS Code)

Dodaj do `.mcp.json` w projekcie lub do konfiguracji globalnej:

``` json
{
  "mcpServers": {
    "loomem": {
      "url": "https://your-loomem.app/mcp",
      "headers": {
        "Authorization": "Bearer loom_your_api_key"
      }
    }
  }
}
```

------------------------------------------------------------------------

<span id="logging"></span>

## Logowanie

<span id="text-logs-default"></span>

### Logi tekstowe (domyślnie)

``` bash
RUST_LOG=debug ./target/release/loomem-server
```

<span id="json-logs-for-grafana-elk"></span>

### Logi JSON (dla Grafana / ELK)

``` bash
LOOMEM_LOG_FORMAT=json ./target/release/loomem-server
```

------------------------------------------------------------------------

<span id="backups"></span>

## Kopie zapasowe

<span id="automatic"></span>

### Automatyczne

Włącz w konfiguracji:

``` toml
[worker.backup]
enabled = true
interval_secs = 43200    # co 12 godzin
max_copies = 2            # zachowaj 2 ostatnie kopie
```

Checkpointy są zapisywane w `{data_dir}/backups/checkpoint-<timestamp>/`. Aby utworzyć kopię zewnętrzną, skopiuj ten katalog lub wykonaj migawkę wolumenu podczas pracy serwera — checkpointy są spójnymi kopiami z danego momentu. Szczegóły, w tym dodatkowe wymagania dla zaszyfrowanych instancji, opisano w [Kopia zapasowa i przywracanie](backup-and-restore.md).

<span id="restore"></span>

### Przywracanie

Zanim podejmiesz jakiekolwiek działania związane z przywracaniem, przeczytaj i wykonaj pełną [procedurę przywracania](backup-and-restore.md#restore-procedure), obejmującą osobny dziennik operacji, klucz główny dla zaszyfrowanych kopii, odbudowę indeksu i weryfikację.

------------------------------------------------------------------------

<span id="upgrading"></span>

## Aktualizacja

1.  Pobierz najnowszy kod: `git pull`
2.  Zbuduj: `cargo build --release -p loomem-server`
3.  Zatrzymaj starą instancję
4.  Uruchom nowy plik wykonywalny

Loomem automatycznie obsługuje migracje schematu:

- RocksDB: zgodność wsteczna (nowe pola otrzymują wartości domyślne)
- Tantivy: automatyczna odbudowa przy niezgodności wersji schematu
- Konfiguracja: nowe klucze są wymagane — sprawdź `config.toml` po aktualizacji

------------------------------------------------------------------------

<span id="ci"></span>

## CI

GitHub Actions uruchamia przy każdym pushu lub PR do `main`:

- `cargo check --workspace`
- `cargo test --workspace --lib`
- `cargo clippy -- -D warnings`
- `cargo fmt --check`

------------------------------------------------------------------------

<span id="security-notes"></span>

## Uwagi dotyczące bezpieczeństwa

- **Nigdy nie wpisuj kluczy API na stałe w `config.toml`** — użyj zamiast tego zmiennej środowiskowej `OPENAI_API_KEY`. Pole `api_key_env` określa, którą zmienną odczytać.
- Pliki `config_snapshot.toml` (tworzone przez przebiegi ewaluacyjne) są ignorowane przez Git, aby zapobiec przypadkowemu wyciekowi sekretów.
- Wszystkie zależności używają liberalnych licencji (MIT, Apache-2.0, BSD, ISC).
