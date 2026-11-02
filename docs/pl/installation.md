<span id="installation-guide"></span>

# Instrukcja instalacji

Loomem jest dostarczany jako trzy samodzielne pliki wykonywalne — `loomem-server`, `loomem-cli` i `loomem-migrate` — z wbudowanymi RocksDB i Tantivy. Wymagają one jedynie systemowych bibliotek uruchomieniowych C i C++. Lokalny model embeddingów pobiera się osobno. Ta instrukcja obejmuje obsługiwane sposoby instalacji, weryfikację, aktualizację i rozwiązywanie problemów.

<span id="supported-platforms"></span>

## Obsługiwane platformy

| Platforma | Cel kompilacji | Gotowy plik |
|----|----|----|
| macOS Apple Silicon | `aarch64-apple-darwin` | ✅ |
| macOS Intel | `x86_64-apple-darwin` | ✅ |
| Linux x86_64 (glibc ≥ 2.35: Ubuntu 22.04+, Debian 12+, Fedora 36+) | `x86_64-unknown-linux-gnu` | ✅ |
| Linux ARM64 (glibc ≥ 2.35) | `aarch64-unknown-linux-gnu` | ✅ |
| Pozostałe (musl, BSD, Windows/WSL2…) | — | kompilacja [ze źródeł](#from-source) |

Instalator wymaga `sh`, `tar` oraz `curl` lub `wget`. Nie wymaga sudo — wszystko trafia do katalogu domowego.

<span id="quick-install"></span>

## Szybka instalacja

Repozytorium jest publiczne, więc poniższe polecenie nie wymaga uwierzytelnienia:

``` bash
curl -fsSL https://raw.githubusercontent.com/vvooki-sys/loomem/main/install.sh | sh
```

Instalator domyślnie wybiera port **3030**, a jeśli jest zajęty, pyta o wolny port. Ustaw `LOOMEM_PORT=NNNN`, aby wybrać port bez pytania.

Instalacja z **prywatnego forka** (z uwierzytelnieniem)

Jeśli Twoja kopia repozytorium jest prywatna, uwierzytelnij się przy użyciu [GitHub CLI](https://cli.github.com):

``` bash
gh auth login            # jednorazowo
gh api repos/<owner>/loomem/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
```

Nie masz `gh`? Możesz też użyć osobistego tokenu dostępu z zakresem `repo`:

``` bash
export GH_TOKEN=ghp_xxx
curl -fsSL -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github.raw" \
  https://api.github.com/repos/<owner>/loomem/contents/install.sh | sh
```

<span id="what-the-installer-does"></span>

### Co robi instalator

1.  Wykrywa system operacyjny i architekturę (`uname`).
2.  Wybiera pierwsze dostępne źródło pobierania: serwer lustrzany `LOOMEM_BASE_URL` → `GH_TOKEN`/`GITHUB_TOKEN` (GitHub API) → zalogowany klient `gh` → publiczne adresy wydań.
3.  Ustala najnowsze wydanie (lub wersję wskazaną przez `LOOMEM_VERSION`).
4.  Pobiera archiwum dla platformy i `SHA256SUMS`, a następnie **sprawdza sumę kontrolną** (niezgodność przerywa instalację).
5.  Instaluje trzy pliki wykonywalne w `~/.loomem/bin`.
6.  Kopiuje szablony konfiguracji (`config.toml`, `entities.toml.example`, `synonyms.toml.example`) do `~/.loomem` i tworzy `entities.toml` na podstawie przykładu (serwer wymaga go przy starcie). **Nie nadpisuje istniejących plików**, więc ponowne uruchomienie jest bezpieczne.
7.  Wyświetla instrukcję konfiguracji PATH, jeśli `~/.loomem/bin` jeszcze w nim nie ma.

Instalator **nie** pobiera lokalnego modelu embeddingów. Pobierz go przed [pierwszym uruchomieniem](#first-run).

<span id="installer-environment-variables"></span>

### Zmienne środowiskowe instalatora

| Zmienna | Domyślnie | Przeznaczenie |
|----|----|----|
| `LOOMEM_VERSION` | najnowsze wydanie | Wskazuje wersję, np. `v0.2.0`. Wymagane dla wersji wstępnych (`v0.2.0-rc1`), które nie są uwzględniane jako najnowsze. |
| `LOOMEM_INSTALL_DIR` | `~/.loomem/bin` | Katalog plików wykonywalnych. |
| `LOOMEM_CONFIG_DIR` | `~/.loomem` | Katalog szablonów konfiguracji. |
| `GH_TOKEN` / `GITHUB_TOKEN` | — | Token GitHub do pobierania z prywatnego repozytorium. |
| `LOOMEM_BASE_URL` | — | Pobiera archiwa ze zwykłego serwera lustrzanego HTTP(S) zamiast GitHuba (wymaga `LOOMEM_VERSION`). |

Przykład: wskazanie wersji i własnego katalogu instalacji:

``` bash
LOOMEM_VERSION=v0.2.0 LOOMEM_INSTALL_DIR=/opt/loomem/bin sh install.sh
```

<span id="path-setup"></span>

## Konfiguracja PATH

Instalator nie zmienia konfiguracji powłoki. Dodaj raz:

``` bash
echo 'export PATH="$HOME/.loomem/bin:$PATH"' >> ~/.zshrc   # lub ~/.bashrc
exec $SHELL
```

<span id="first-run"></span>

## Pierwsze uruchomienie

``` bash
curl -fsSL https://raw.githubusercontent.com/vvooki-sys/loomem/main/scripts/fetch-embedding-model.sh | sh   # lokalny model embeddingów, wymagany
export OPENAI_API_KEY="sk-..."     # lub: export LOOMEM_ALLOW_KEYLESS=1
cd ~/.loomem
loomem-server
```

Serwer nie uruchomi się bez lokalnego modelu embeddingów (około 113 MB, jednorazowo pobieranego do `~/.loomem/models`). Domyślna konfiguracja wymaga też klucza LLM albo ustawienia `LOOMEM_ALLOW_KEYLESS=1` dla trybu bez klucza. Funkcje wyłączane w tym trybie opisano w sekcji [Kompilacja ze źródeł](#from-source).

Serwer **wymaga** plików `config.toml` i `entities.toml` w katalogu roboczym. Instalator tworzy oba, więc uruchomienie z `~/.loomem` spełnia ten warunek. Edytuj `entities.toml`, aby podać Loomem osoby, projekty i aliasy (patrz komentarze w pliku).

Serwer nasłuchuje na `http://127.0.0.1:3030` i zapisuje dane w `./data` względem katalogu roboczego. Uruchomienie z `~/.loomem` umieszcza dane w `~/.loomem/data`. Sprawdź:

``` bash
curl http://localhost:3030/health
```

Następnie połącz klienta MCP. Instrukcje dla Claude Code, claude.ai, ChatGPT i OpenClaw znajdują się w [README](https://github.com/vvooki-sys/loomem#connect-an-mcp-client).

**Bezpieczeństwo:** uwierzytelnianie jest domyślnie wyłączone na potrzeby localhost. Jeśli serwer jest dostępny dla innych osób, ustaw `LOOMEM_AUTH_TOKEN` (patrz [model bezpieczeństwa](SECURITY.md) i [wdrożenie](deployment.md)).

<span id="verifying-checksums-manually"></span>

## Ręczna weryfikacja sum kontrolnych

Instalator weryfikuje je automatycznie. Aby sprawdzić je ręcznie:

``` bash
gh release download v0.2.0 --repo vvooki-sys/loomem \
  --pattern 'loomem-0.2.0-aarch64-apple-darwin.tar.gz' --pattern 'SHA256SUMS'
shasum -a 256 -c --ignore-missing SHA256SUMS    # macOS
sha256sum -c --ignore-missing SHA256SUMS        # Linux
```

<span id="upgrading"></span>

## Aktualizacja

Uruchom instalator ponownie — pliki wykonywalne zostaną zastąpione, a konfiguracja pozostanie bez zmian:

``` bash
gh api repos/vvooki-sys/loomem/contents/install.sh -H "Accept: application/vnd.github.raw" | sh
```

Najpierw sprawdź [CHANGELOG](https://github.com/vvooki-sys/loomem/blob/main/CHANGELOG.md) pod kątem zmian niekompatybilnych. Jeżeli wydanie wymaga migracji formatu danych, zatrzymaj serwer i uruchom `loomem-migrate` przed uruchomieniem nowej wersji.

<span id="uninstalling"></span>

## Odinstalowanie

Loomem przechowuje swoje pliki w katalogach instalacji i danych:

``` bash
rm -rf ~/.loomem          # pliki wykonywalne, konfiguracja i dane (przy uruchomieniu z ~/.loomem)
```

Jeśli dodano wpis PATH do konfiguracji powłoki, usuń go. Jeśli serwer był uruchamiany z innego katalogu roboczego, dane `./data` znajdują się tam.

<span id="from-source"></span>

## Kompilacja ze źródeł

``` bash
git clone https://github.com/vvooki-sys/loomem.git
cd loomem
cp entities.toml.example entities.toml
(cd loomem-dashboard && npm ci && npm run build)   # dashboard osadzany podczas kompilacji
./scripts/fetch-embedding-model.sh                 # lokalny model embeddingów → ~/.loomem/models
cargo build --release -p loomem-server -p loomem-cli -p loomem-migrate
# pliki wykonywalne w target/release/
```

Wymagane są Rust (stable), Node.js (dla panelu) oraz libclang do kompilacji RocksDB: `apt install libclang-dev` w Debianie/Ubuntu; na macOS biblioteka jest dostępna w Xcode Command Line Tools. W systemie Windows zobacz [INSTALL_WINDOWS.md](https://github.com/vvooki-sys/loomem/blob/main/INSTALL_WINDOWS.md).

Pominięcie budowy panelu spowoduje, że panel zwróci błąd 404. Lokalny model embeddingów jest **wymagany**: przy domyślnym `embedding_provider = "local"` serwer nie uruchomi się bez niego (`refusing cloud fallback`), niezależnie od obecności klucza API. Domyślna konfiguracja włącza także ekstrakcję i dreaming oparte na LLM, więc serwer **nie uruchomi się** bez klucza LLM (`export OPENAI_API_KEY=...`) albo ustawienia `LOOMEM_ALLOW_KEYLESS=1`. W trybie bez klucza `memory_ingest` może zapisać tekst źródłowy, lecz zwraca błąd i nie wyodrębnia faktów. Ekstrakcja encji jest wyłączona, a dreaming kończy się błędem. `memory_store` i wyszukiwanie nadal działają.

<span id="docker"></span>

## Docker

``` bash
docker build -t loomem .
export LOOMEM_AUTH_TOKEN=$(openssl rand -hex 32)
export LOOMEM_AT_REST_MASTER_KEY=$(openssl rand -base64 32)
export OPENAI_API_KEY="sk-..."
docker run -p 3030:3030 -v loomem-data:/data \
  -e LOOMEM_AUTH_TOKEN -e LOOMEM_AT_REST_MASTER_KEY -e OPENAI_API_KEY \
  -e LOOMEM_EMBEDDING_PROVIDER=openai -e LOOMEM_EMBEDDING_DIM=1536 loomem
```

Obraz nasłuchuje na `0.0.0.0`, więc wymaga tokenu uwierzytelniającego oraz domyślnie klucza głównego szyfrowania danych w spoczynku — bez nich serwer nie wystartuje. Świadome wyłączenie zabezpieczeń: `LOOMEM_ALLOW_UNAUTH=1`, `LOOMEM_AT_REST_EXPECT_ENABLED=0`. Opcje reverse proxy, TLS i chmury opisano w [instrukcji wdrożenia](deployment.md), a model bezpieczeństwa w [opisie bezpieczeństwa](SECURITY.md).

Obraz nie zawiera lokalnego modelu embeddingów, dlatego polecenie przełącza embeddingi na OpenAI (`LOOMEM_EMBEDDING_PROVIDER=openai`, `LOOMEM_EMBEDDING_DIM=1536`) i przekazuje `OPENAI_API_KEY`. Zapisywany tekst jest wtedy wysyłany do API OpenAI. Bez tych zmiennych serwer nie uruchomi się (`refusing cloud fallback`).

<span id="troubleshooting"></span>

## Rozwiązywanie problemów

| Objaw | Przyczyna i rozwiązanie |
|----|----|
| `could not determine the latest release` | Repozytorium jest prywatne, a Ty nie masz uwierzytelnienia → `gh auth login` lub ustaw `GH_TOKEN`. Jeśli jedyne wydanie jest wersją wstępną, wskaż `LOOMEM_VERSION=v0.2.0-rc1`. |
| `download failed: … If this repo is private…` | Podobnie jak wyżej — instalator nie ma dostępu do plików wydania bez uwierzytelnienia. |
| `checksum mismatch` | Pobrany plik jest uszkodzony lub zmieniony. Spróbuj ponownie. Jeśli błąd się powtarza, **nie instaluj** — porównaj z `SHA256SUMS` na stronie wydania i zgłoś problem. |
| `SHA256SUMS could not be downloaded — refusing to install an unverified archive` | Nie można pobrać pliku sum kontrolnych. Instalator przerywa instalację; sprawdź dostępność pliku w wybranym wydaniu lub serwerze lustrzanym. |
| `unsupported OS/architecture` | Brak gotowego pliku dla Twojej platformy — [skompiluj ze źródeł](#from-source). |
| `version 'GLIBC_2.35' not found` (lub podobny) | Wersja glibc w Twojej dystrybucji jest starsza od wymaganej — [skompiluj ze źródeł](#from-source) albo użyj [Dockera](#docker). |
| `loomem-server: command not found` po instalacji | `~/.loomem/bin` nie znajduje się w PATH — zobacz [konfigurację PATH](#path-setup). |
| `gh: Not Found (HTTP 404)` przy poleceniu `gh api` | Twoje konto GitHub nie ma dostępu do prywatnego repozytorium albo jesteś zalogowany na inne konto (`gh auth status`). |
| `Local embedding provider could not load its model; refusing cloud fallback` | Lokalnego modelu embeddingów brakuje lub nie można go odczytać. Pobierz go zgodnie z sekcją [Pierwsze uruchomienie](#first-run) albo ustaw `embedding_model_path` w `config.toml` na katalog modelu. |
| `… enabled but no LLM API key found` | Domyślna konfiguracja wymaga klucza LLM. Ustaw `OPENAI_API_KEY` lub uruchom serwer z `LOOMEM_ALLOW_KEYLESS=1`, aby użyć trybu bez klucza. |
| `Error: Failed to load entities file (entities.toml)` | Serwer wymaga `entities.toml` w katalogu roboczym — wykonaj `cp ~/.loomem/entities.toml.example ~/.loomem/entities.toml` i uruchom go z `~/.loomem`. |
| Serwer startuje, ale `curl /health` nie działa | Podczas pierwszego uruchomienia tworzone są indeksy, co może potrwać kilka sekund. Port 3030 jest zajęty? Sprawdź log serwera. Adres i port nasłuchu znajdują się w `config.toml`. |
