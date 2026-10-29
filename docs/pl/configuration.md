<span id="configuration-reference"></span>

# Konfiguracja

Wszystkie ustawienia znajdują się w pliku `config.toml`. Nie ma wartości domyślnych zapisanych na stałe — każdą wartość trzeba jawnie skonfigurować.

Ścieżkę pliku konfiguracji można zmienić za pomocą zmiennej środowiskowej `LOOMEM_CONFIG`.

Uwierzytelnianie konfiguruje się przez `[server].auth_token_env` (zobacz [\[server\]](#server)) — nie ma osobnej sekcji `[auth]`.

------------------------------------------------------------------------

<span id="storage"></span>

## \[storage\]

``` toml
[storage]
data_dir = "./data"
vector_enabled = true
```

| Klucz | Typ | Domyślnie | Opis |
|----|----|----|----|
| `data_dir` | string | "./data" | Katalog główny RocksDB, Tantivy i embeddingów |
| `vector_enabled` | bool | true | Włącz wyszukiwanie wektorowe (embeddingi) |

<span id="storagerocksdb"></span>

### \[storage.rocksdb\]

``` toml
[storage.rocksdb]
max_open_files = 1000
compression = "lz4"
write_buffer_size = 67108864
max_write_buffer_number = 3
```

| Klucz                     | Typ    | Opis                                  |
|---------------------------|--------|---------------------------------------|
| `max_open_files`          | int    | Rozmiar puli deskryptorów plików      |
| `compression`             | string | `"lz4"` lub `"snappy"` — zalecane LZ4 |
| `write_buffer_size`       | int    | Liczba bajtów na bufor zapisu (64 MB) |
| `max_write_buffer_number` | int    | Liczba buforów zapisu                 |

<span id="storagetantivy"></span>

### \[storage.tantivy\]

``` toml
[storage.tantivy]
enabled = true
heap_size_mb = 128
```

| Klucz          | Typ  | Opis                                  |
|----------------|------|---------------------------------------|
| `enabled`      | bool | Włącz wyszukiwanie pełnotekstowe BM25 |
| `heap_size_mb` | int  | Pula pamięci zapisu indeksu           |

<span id="storageintent_log"></span>

### \[storage.intent_log\]

Dziennik zapisu z wyprzedzeniem zapewniający spójność między magazynami.

``` toml
[storage.intent_log]
enabled = true
dir = "wal"
max_size_mb = 10
sync_on_write = false
```

| Klucz | Typ | Opis |
|----|----|----|
| `enabled` | bool | Włącz WAL |
| `dir` | string | Katalog WAL (względem data_dir) |
| `max_size_mb` | int | Rotuj po osiągnięciu tego rozmiaru dziennika |
| `sync_on_write` | bool | fsync przy każdym zapisie (bezpieczniej, lecz wolniej) |

------------------------------------------------------------------------

<span id="search"></span>

## \[search\]

``` toml
[search]
top_k = 10
max_request_top_k = 1000      # HTTP top_k musi mieścić się w zakresie 1..ten limit; MCP ma osobne limity
surprise_boost = 1.5
synonyms_file = "synonyms.toml"
entities_file = "entities.toml"
rerank_enabled = false
rerank_candidates = 20
rerank_model_dir = "models/reranker"
multi_query_enabled = false
stem_polish = true
evidence_fields = false
```

| Klucz | Typ | Opis |
|----|----|----|
| `top_k` | int | Domyślna liczba wyników wyszukiwania |
| `max_request_top_k` | int | Górna granica liczby wyników HTTP (domyślnie 1000); zero lub żądanie powyżej limitu zwraca 400 przed wyszukiwaniem. Limity MCP są oddzielne. |
| `surprise_boost` | f64 | Mnożnik nowości przy ingest (inspirowany Titans) |
| `synonyms_file` | string | Ścieżka do mapy rozszerzania synonimów |
| `entities_file` | string | Ścieżka do słownika encji |
| `rerank_enabled` | bool | Włącz reranking cross-encoderem (~97 ms/parę) |
| `rerank_candidates` | int | Przekaż najlepsze N wyników do rerankera |
| `rerank_model_dir` | string | Katalog modelu ONNX |
| `multi_query_enabled` | bool | Rozbijaj złożone zapytania na zapytania składowe |
| `stem_polish` | bool | Włącz stemming języka polskiego |
| `evidence_fields` | bool | **Eksperymentalne, wymaga włączenia** (domyślnie `false`). Przy `true` każdy wynik `/v1/search` oraz każdy element `GET /api/memories/{id}` zawiera dodatkowy obiekt `evidence` (`author`, `conversation_date`, `event_date`, `event_date_provenance`, `source_id`, `byte_start`, `byte_end`), a każdy wiersz `memory_search` zawiera przed wynikiem znacznik dowodu w nawiasach. Przy `false` odpowiedzi są identyczne bajt po bajcie z odpowiedziami sprzed wprowadzenia flagi. Zobacz [dokumentację API](api-reference.md#post-v1search). |

<span id="searchcache"></span>

### \[search.cache\]

``` toml
[search.cache]
enabled = true
max_entries = 500
ttl_secs = 300
```

Semantyczna pamięć podręczna zapytań. Identyczne zapytania w czasie TTL zwracają zapisane wyniki.

<span id="searchgraph"></span>

### \[search.graph\]

``` toml
[search.graph]
enabled = true
max_hops = 1
boost_factor = 0.3
max_graph_additions = 3
```

| Klucz | Typ | Opis |
|----|----|----|
| `enabled` | bool | Włącz wyszukiwanie wzbogacone grafem |
| `max_hops` | int | 1 = bezpośredni sąsiedzi, 2 = dwa kroki |
| `boost_factor` | f64 | Mnożnik wyniku dla elementów znalezionych przez graf (0.0 – 1.0) |
| `max_graph_additions` | int | Maksymalna liczba wyników wyłącznie z grafu dodanych do wyszukiwania |

<span id="searchhybrid_weights"></span>

### \[search.hybrid_weights\]

``` toml
[search.hybrid_weights]
vector = 0.6
bm25 = 0.4
```

Steruje proporcją łączenia wyników. Suma musi wynosić 1.0.

<span id="searchdecay"></span>

### \[search.decay\]

``` toml
[search.decay]
l0_lambda = 0.05
l1_lambda = 0.03
```

Wykładnicze tempo zanikania dla każdego poziomu. Większa wartość = szybsze zanikanie. Okres półtrwania ≈ ln(2) / lambda dni.

| Poziom | Lambda | Przybliżony okres półtrwania |
|--------|--------|------------------------------|
| L0     | 0.05   | ~14 dni                      |
| L1     | 0.03   | ~23 dni                      |

<span id="searchcomplexity"></span>

### \[search.complexity\]

``` toml
[search.complexity]
enabled = false
simple_top_k = 3
medium_top_k = 10
complex_top_k = 20
```

Routing uwzględniający złożoność. Po włączeniu dostosowuje `top_k` do klasyfikacji złożoności zapytania. Obecnie wyłączony — wszystkie zapytania korzystają z pełnego procesu.

------------------------------------------------------------------------

<span id="worker"></span>

## \[worker\]

<span id="workerconsolidation"></span>

### \[worker.consolidation\]

``` toml
[worker.consolidation]
interval_secs = 300
batch_size = 200
concurrency = 2
timeout_secs = 300
min_chunks_to_consolidate = 3
min_age_secs = 60
prompt_version = 1
consolidation_style = "structured"
similarity_threshold = 0.20
```

| Klucz | Typ | Opis |
|----|----|----|
| `interval_secs` | int | Częstotliwość uruchamiania (5 min) |
| `batch_size` | int | Maksymalna liczba fragmentów na strumień w jednym przebiegu |
| `concurrency` | int | Równoległe strumienie |
| `min_chunks_to_consolidate` | int | Pomiń strumienie z mniejszą liczbą fragmentów |
| `min_age_secs` | int | Konsoliduj tylko starsze fragmenty |
| `consolidation_style` | string | `"structured"` = fakty z typem i ustalaniem dat, `"observation"` = szczegółowe fakty, `"summary"` = akapit |
| `similarity_threshold` | f64 | Podobieństwo cosinusowe grupowania tematów przed kompresją (0.0 = jeden na fragment, 1.0 = wszystkie razem) |

<span id="workerdecay_worker"></span>

### \[worker.decay_worker\]

``` toml
[worker.decay_worker]
interval_secs = 3600
factor = 0.995
l0_factor = 0.990
l1_factor = 0.995
dormant_threshold = 0.01
access_boost = true
adaptive_enabled = true
adaptive_dampening = 0.5
adaptive_cap = 200
```

| Klucz | Typ | Opis |
|----|----|----|
| `interval_secs` | int | Uruchamiaj co godzinę |
| `l0/l1_factor` | f64 | Godzinowy mnożnik zanikania dla poziomu |
| `dormant_threshold` | f64 | Wynik poniżej progu oznacza stan uśpienia |
| `access_boost` | bool | Resetuj wynik po trafieniu wyszukiwania |
| `adaptive_enabled` | bool | ACT-R: częściej używane elementy zanikają wolniej |
| `adaptive_dampening` | f64 | Wpływ access_count na zanikanie |
| `adaptive_cap` | int | Górna granica access_count dla efektu adaptacyjnego |

<span id="workercompaction"></span>

### \[worker.compaction\]

``` toml
[worker.compaction]
interval_secs = 3600
timeout_secs = 300
```

Wyzwalanie kompakcji RocksDB w tle.

<span id="workerclustering"></span>

### \[worker.clustering\]

``` toml
[worker.clustering]
interval_secs = 21600
max_iterations = 1000
timeout_secs = 600
```

Proces grupowania uruchamia k-means na embeddingach L1, aby grupować powiązane wspomnienia.

------------------------------------------------------------------------

<span id="scheduler"></span>

## \[scheduler\]

``` toml
[scheduler]
enabled = true
```

Główny przełącznik wszystkich procesów w tle.

------------------------------------------------------------------------

<span id="llm"></span>

## \[llm\]

``` toml
[llm]
provider = "openai"                  # generowanie odpowiedzi (konsolidacja / reflect)
api_key_env = "OPENAI_API_KEY"
embedding_provider = "local"         # "local" (ONNX na urządzeniu) lub "openai"
embedding_model = "text-embedding-3-small"   # używany, gdy embedding_provider = "openai"
# embedding_model_path = "/abs/path"  # katalog modelu lokalnego; domyślnie ~/.loomem/models/multilingual-e5-small
embedding_dim = 384                  # MUSI odpowiadać aktywnemu modelowi embeddingów
compression_model = "gpt-4.1-mini"
timeout_secs = 10
fallback_to_regex = true
```

| Klucz | Typ | Opis |
|----|----|----|
| `provider` | string | Dostawca generowania tekstu do konsolidacji / reflect (`"openai"`) |
| `embedding_provider` | string | `"local"` (ONNX na urządzeniu, bez klucza API) lub `"openai"`. Brak w starszej konfiguracji → traktowany jako `"openai"` |
| `api_key_env` | string | Nazwa zmiennej środowiskowej z kluczem API |
| `embedding_model` | string | Nazwa modelu embeddingów przy `embedding_provider = "openai"` |
| `embedding_model_path` | string? | Katalog z `model.onnx` i `tokenizer.json` dla lokalnych embeddingów. Brak ustawienia → `~/.loomem/models/multilingual-e5-small` |
| `embedding_dim` | int | Wymiar wektora embeddingu — **musi odpowiadać aktywnemu modelowi** |
| `compression_model` | string | Model do konsolidacji, ekstrakcji i dream |
| `timeout_secs` | int | Limit czasu wywołania API |
| `fallback_to_regex` | bool | Tylko konsolidacja: kompresja regex po błędzie wywołania API generowania tekstu (nie obejmuje braku klucza) |

<span id="local-embeddings-default"></span>

### Lokalne embeddingi (domyślnie)

Nowe instalacje korzystają z **`embedding_provider = "local"`**: embeddingi są obliczane na urządzeniu przez model ONNX bez kwantyzacji (domyślnie `multilingual-e5-small`, 384 wymiary, dobra skuteczność wyszukiwania wielojęzycznego i polskiego) przy użyciu środowiska `tract` napisanego w Rust. Klucz API nie jest wymagany, a tekst nie opuszcza urządzenia. Obsługa modelu jest w domyślnej kompilacji; pliki modelu pobierzesz poleceniem:

``` sh
./scripts/fetch-embedding-model.sh         # → ~/.loomem/models/multilingual-e5-small
```

LLM generujący tekst (konsolidacja, ekstrakcja faktów, dream) działa niezależnie. Przy domyślnej konfiguracji (te podsystemy są włączone) i bez `OPENAI_API_KEY` serwer nie uruchomi się, o ile nie ustawisz `LOOMEM_ALLOW_KEYLESS=1`. Wtedy `memory_ingest` może zapisać tekst źródłowy, ale zwraca błąd braku klucza i nie wyodrębnia faktów. Ekstrakcja encji przez LLM jest wyłączona, a dream i konsolidacja kończą się błędem; `memory_store` i wyszukiwanie semantyczne działają całkowicie lokalnie. Wyłączenie tych podsystemów w `config.toml` usuwa kontrolę przy starcie, a przy `knowledge_extraction.enabled = false` `memory_ingest` zapisuje surową transkrypcję. Aby używać lokalnego LLM do generowania tekstu, skieruj `provider` do punktu końcowego zgodnego z OpenAI (przyszły etap).

<span id="embedding-dimension-re-embedding"></span>

### Wymiar embeddingów i ponowne obliczanie

`embedding_dim` **musi** odpowiadać aktywnemu modelowi embeddingów (`multilingual-e5-small` = 384, OpenAI `text-embedding-3-small` = 1536). Wymiar użyty przy tworzeniu bazy jest zapisany w jej metadanych; w razie niezgodności serwer **odmawia uruchomienia**, zamiast po cichu mieszać wymiary wektorów. Aby zmienić dostawcę lub model istniejącej bazy, przelicz embeddingi:

``` sh
loomem-server --reembed   # przelicz wektory skonfigurowanym dostawcą (uruchom po zatrzymaniu serwera)
```

Aby korzystać z API zamiast lokalnych embeddingów, ustaw `embedding_provider = "openai"` i `embedding_dim = 1536`, wyeksportuj `OPENAI_API_KEY` i przelicz embeddingi.

------------------------------------------------------------------------

<span id="server"></span>

## \[server\]

``` toml
[server]
host = "127.0.0.1"
port = 3030
auth_token_env = "LOOMEM_AUTH_TOKEN"
```

| Klucz | Typ | Opis |
|----|----|----|
| `host` | string | Adres nasłuchiwania (`0.0.0.0` w środowisku produkcyjnym) |
| `port` | int | Port HTTP (zmieniany przez zmienną środowiskową `PORT`) |
| `auth_token_env` | string | Nazwa zmiennej środowiskowej zawierającej klucz Bearer API (domyślnie `LOOMEM_AUTH_TOKEN`). Jeśli zmienna nie jest ustawiona lub jest pusta, uwierzytelnianie jest wyłączone, a serwer działa w lokalnym trybie passthrough |

------------------------------------------------------------------------

<span id="resource_guards"></span>

## \[resource_guards\]

``` toml
[resource_guards]
max_cpu_cores = 1.0
max_memory_mb = 512
min_disk_space_mb = 1024
llm_timeout_secs = 30
worker_timeout_secs = 120
```

Kontrole przy uruchamianiu. Serwer odmówi startu, jeśli zasoby są poniżej progów.

------------------------------------------------------------------------

<span id="streams-and-namespaces"></span>

## \[streams\] i \[namespaces\]

``` toml
[streams]
shared = "001"

[streams.agents]
assistant = { raw = "100", compressed = "101" }

[namespaces]
personal = "100"
work = "110"
```

`streams` — identyfikatory surowych i skompresowanych strumieni przypisane agentom. `namespaces` — mapowanie czytelnej nazwy na ID strumienia, zwracane przez `memory_namespaces`.

Dane zapisane bez wskazania strumienia trafiają do wbudowanego strumienia domyślnego `__user_default__`.

------------------------------------------------------------------------

<span id="pii"></span>

## \[pii\]

``` toml
[pii]
enabled = true
redact_phones = true
redact_emails = true
redact_ids = true
blocklist_file = "pii_blocklist.txt"
audit_log = true
```

Dane osobowe są usuwane przed **każdym** wywołaniem API LLM (konsolidacja, ekstrakcja, dream). Oryginalna treść w RocksDB pozostaje bez zmian.

------------------------------------------------------------------------

<span id="cost"></span>

## \[cost\]

``` toml
[cost]
daily_cap_usd = 15.00
alert_threshold_usd = 10.00
anomaly_multiplier = 3.0
persist = true
```

| Klucz | Typ | Opis |
|----|----|----|
| `daily_cap_usd` | f64 | Twardy limit — po jego przekroczeniu nie ma wywołań LLM |
| `alert_threshold_usd` | f64 | Próg ostrzegawczy |
| `anomaly_multiplier` | f64 | 3 razy typowy koszt dzienny = alert anomalii |
| `persist` | bool | Zachowaj liczniki kosztów po ponownym uruchomieniu |

------------------------------------------------------------------------

<span id="knowledge_extraction"></span>

## \[knowledge_extraction\]

``` toml
[knowledge_extraction]
enabled = true
model = "gpt-4.1-mini"
max_transcript_tokens = 20000
dedup_cosine_threshold = 0.92
contradiction_check = true
contradiction_cosine_min = 0.5
max_facts_per_transcript = 20
max_tokens = 2000
input_chunk_tokens = 0
split_on_truncation = false
```

Steruje procesem LLM `memory_ingest`.

| Klucz | Typ | Domyślnie | Opis |
|----|----|----|----|
| `max_tokens` | int | 2000 | Limit odpowiedzi na wywołanie ekstrakcji. Odpowiedź, która go osiągnie (`finish_reason: "length"`), jest odrzucana; nigdy nie jest przyjmowana częściowo |
| `input_chunk_tokens` | int | 0 | Limit wejścia dla jednego wywołania ekstrakcji (4 znaki ≈ 1 token). `0` zachowuje poprzedni proces: jedno wywołanie na transkrypcję do `max_transcript_tokens`. Inna wartość dzieli transkrypcję na granicach wierszy na fragmenty o maksymalnie `min(input_chunk_tokens, max_transcript_tokens)` tokenach i wykonuje jedno wywołanie dla każdego |
| `split_on_truncation` | bool | false | Przy `true` fragment, którego odpowiedź została obcięta, jest ponownie przetwarzany jako dwie połowy rozdzielone na granicy wiersza (głębokość 2, maksymalnie 4 dodatkowe wywołania na fragment). Przy `false` obcięty fragment jest odrzucany i zgłaszany bez ponawiania |

Te trzy klucze są dodatkowymi ustawieniami: z powyższymi wartościami domyślnymi ekstrakcja wykonuje te same wywołania co przed ich wprowadzeniem. Obcięta odpowiedź nie jest przyjmowana w żadnym trybie.

------------------------------------------------------------------------

<span id="fragments"></span>

## \[fragments\]

**Eksperymentalne, wymaga włączenia.** Domyślnie wyłączone; istniejące wdrożenia i klienci zachowują działanie identyczne bajt po bajcie.

``` toml
[fragments]
enabled = false
max_fragment_tokens = 400
prefix_reserve_tokens = 16
provenance_role = "claim"
token_counter = "embedder"
```

| Klucz | Typ | Domyślnie | Opis |
|----|----|----|----|
| `enabled` | bool | false | Zapisuj fragmenty źródłowe przy `memory_ingest` |
| `max_fragment_tokens` | int | 400 | Limit tokenów tekstu fragmentu mierzony przez `token_counter`. Musi być dodatni |
| `prefix_reserve_tokens` | int | 16 | Zapas ponad limit tekstu. Lokalny model embeddingów dodaje do treści dokładnie dwa tokeny specjalne (bez prefiksu `passage:`); reszta pokrywa różnicę między wybranym licznikiem a tokenizerem modelu embeddingów |
| `provenance_role` | string | `"claim"` | Rola przypisywana każdemu fragmentowi: `"claim"` (pełna waga), `"evidence"` (×0.5) lub `"cue"` (×0.8). Każda inna wartość powoduje błąd wczytania konfiguracji |
| `token_counter` | string | `"embedder"` | `"embedder"` (domyślny i zalecany: tokenizer załadowanego lokalnego modelu embeddingów, jedyny licznik używający jego limitu), `"approx"` (`ceil(chars / 3)`, bez modelu) lub `"tiktoken"` (BPE `cl100k_base`) — dwa liczniki bez modelu służą do nielokalnych dostawców embeddingów i testów offline. Inna wartość powoduje błąd wczytania konfiguracji |

Po włączeniu `memory_ingest` zapisuje, zaraz po źródle całej rozmowy i przed ekstrakcją, jeden fragment na turę rozmowy (długie tury są dalej dzielone na granicach akapitów, zdań i białych znaków). Każdy fragment zawiera rolę autora, datę rozmowy i dokładny zakres bajtów w zapisanym źródle. Fragmenty mają od początku status `consolidated` (nigdy nie są wejściem konsolidacji ani dream), są wykluczone z sekcji `recent` w `memory_context`, nie mają własnej daty zdarzenia (wiersz `memory_search` pokazuje datę źródła `observed:`/`recorded:`) i są usuwane wraz ze źródłem. Do wyniku narzędzia dodawane jest zdanie ` Stored N fragments.` (zobacz [narzędzia MCP](mcp-tools.md#memory_ingest)). Zapisy fragmentów są dodatkowe i nigdy nie blokują ekstrakcji faktów.

Walidacja przy uruchomieniu: `max_fragment_tokens` musi być dodatnie, a przy `embedding_provider = "local"` suma `max_fragment_tokens + prefix_reserve_tokens` nie może przekraczać okna lokalnego modelu embeddingów wynoszącego 512 tokenów. `enabled = true` z `token_counter = "embedder"` (domyślnie) wymaga `embedding_provider = "local"` — przy innym dostawcy ustaw jawnie `token_counter = "approx"` albo `"tiktoken"`, w przeciwnym razie wczytanie konfiguracji zawiedzie. Przy `token_counter = "embedder"` bez załadowanego modelu lokalnego segmentacja kończy się błędem i tekst `memory_ingest` o tym informuje (nie ma cichego przejścia na przybliżenie).

`provenance_role` wpływa na ranking tylko przy wyszukiwaniu łączonym (z włączonymi wektorami, dla zapytań nieprostych). Ścieżka wyłącznie BM25 nie stosuje mnożnika pochodzenia, dlatego dla krótkich zapytań fragmenty `claim` i `evidence` otrzymują ten sam wynik.

------------------------------------------------------------------------

<span id="entity_extraction"></span>

## \[entity_extraction\]

``` toml
[entity_extraction]
enabled = true
model = "gpt-4.1-mini"
batch_size = 20
flush_interval_secs = 3
queue_capacity = 200
confidence_threshold = 0.7
max_tokens_per_batch = 2000
```

Asynchroniczne rozpoznawanie encji przez LLM dla encji spoza słownika.

------------------------------------------------------------------------

<span id="contradiction"></span>

## \[contradiction\]

``` toml
[contradiction]
enabled = true
similarity_threshold = 0.70
max_candidates = 5
model = "gpt-4.1-mini"
```

Gdy pojawia się nowy fakt, LLM sprawdza najlepsze `max_candidates` podobnych istniejących faktów pod kątem sprzeczności. Wynik: `updates` (zastępuje), `extends` (zachowuje oba), `none` (brak relacji).

------------------------------------------------------------------------

<span id="conversation_extraction"></span>

## \[conversation_extraction\]

``` toml
[conversation_extraction]
enabled = true
model = "gpt-4.1-mini"
max_tokens = 2000
confidence_threshold = 0.7
dedup_threshold = 0.80
max_extractions_per_request = 30
```

------------------------------------------------------------------------

<span id="profile"></span>

## \[profile\]

``` toml
[profile]
enabled = true
model = "gpt-4.1-mini"
max_chunks = 100
cache_ttl_secs = 3600
max_static_facts = 30
max_recent_items = 15
```

Synteza profilu użytkownika. Przechowywana w pamięci podręcznej przez `cache_ttl_secs`.

------------------------------------------------------------------------

<span id="retention"></span>

## \[retention\]

``` toml
[retention]
soft_delete_days = 30
hard_purge_interval_secs = 86400
```

| Klucz | Typ | Opis |
|----|----|----|
| `soft_delete_days` | int | Liczba dni przechowywania miękko usuniętych fragmentów przed trwałym usunięciem |
| `hard_purge_interval_secs` | int | Częstotliwość uruchamiania procesu usuwania (sekundy) |

Proces trwałego usuwania usuwa przeterminowane, miękko usunięte fragmenty z RocksDB, Tantivy i grafu.

------------------------------------------------------------------------

<span id="associator"></span>

## \[associator\]

``` toml
[associator]
enabled = true
k_clusters = 0
max_clusters = 50
max_iterations = 100
min_serendipity = 0.1
max_associations = 3
```

| Klucz | Typ | Opis |
|----|----|----|
| `enabled` | bool | Włącz silnik nieoczekiwanych skojarzeń ECA i automatyczne grupowanie |
| `k_clusters` | int | Stała liczba klastrów (0 = automatycznie) |
| `max_clusters` | int | Górna granica automatycznie określanej liczby klastrów |
| `min_serendipity` | f64 | Minimalny wynik potrzebny do uwzględnienia skojarzenia |
| `max_associations` | int | Maksymalna liczba nieoczekiwanych wyników na wyszukiwanie |

------------------------------------------------------------------------

<span id="dream"></span>

## \[dream\]

``` toml
[dream]
enabled = true
batch_size = 50
min_group_size = 2
model = "gpt-4.1-mini"
cost_cap_usd_per_run = 0.10
```

| Klucz | Typ | Opis |
|----|----|----|
| `batch_size` | int | Liczba fragmentów na przebieg dream |
| `min_group_size` | int | Minimalna liczba fragmentów tematu do połączenia |
| `cost_cap_usd_per_run` | f64 | Limit bezpieczeństwa na sesję dream |

------------------------------------------------------------------------

<span id="memory_generator"></span>

## \[memory_generator\]

``` toml
[memory_generator]
enabled = true
max_chunks = 200
max_sections = 20
model = "gpt-4.1-mini"
```

Steruje wynikiem punktu końcowego `generate-memory-md`.

------------------------------------------------------------------------

<span id="environment-variables"></span>

## Zmienne środowiskowe

| Zmienna | Przeznaczenie | Wymagana |
|----|----|----|
| `LOOMEM_AUTH_TOKEN` | Klucz Bearer API (nazwa konfigurowana przez `server.auth_token_env`) | Nie (bez ustawienia uwierzytelnianie wyłączone — lokalny tryb passthrough) |
| `OPENAI_API_KEY` | Klucz API LLM (nazwa konfigurowana przez `llm.api_key_env`) | Tylko dla generowania tekstu OpenAI (`provider = "openai"`) lub embeddingów OpenAI (`embedding_provider = "openai"`). Przy lokalnych embeddingach i bez klucza domyślna konfiguracja uruchamia się tylko z `LOOMEM_ALLOW_KEYLESS=1`, a etapy LLM są niedostępne (`memory_store` i wyszukiwanie nadal działają). |
| `LOOMEM_CONFIG` | Ścieżka do pliku konfiguracji | Nie (domyślnie: `config.toml`) |
| `PORT` | Nadpisz port serwera | Nie |
| `SERVER_ORIGIN` | Bazowy URL przekierowania OAuth | Nie (dla MCP Remote) |
| `LOOMEM_LOG_FORMAT` | `"json"` dla dzienników strukturalnych | Nie |
| `LOOMEM_AT_REST_MASTER_KEY` | Klucz główny włączający szyfrowanie danych w spoczynku (32 bajty w base64) | Nie |
| `LOOMEM_AT_REST_EXPECT_ENABLED` | Po ustawieniu odmów uruchomienia bez klucza głównego | Nie |
| `LOOMEM_AMBIENT_CACHE_TTL_SECS` | TTL pamięci podręcznej dla odpowiedzi `/v1/ambient` | Nie (domyślnie 60) |
| `TELEGRAM_BOT_TOKEN`, `LOOMEM_TELEGRAM_CHAT_ID` | Opcjonalny webhook alertów o kosztach | Nie |

Informacje o szyfrowaniu danych w spoczynku: [SECURITY.md — dane w spoczynku](SECURITY.md#data-at-rest).
