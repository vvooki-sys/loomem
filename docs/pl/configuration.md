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

<span id="quality-levers"></span>

### Dźwignie jakości

``` toml
[search]
rerank_date_prefix = false
rerank_llm_max_chars = 200
rerank_onnx_max_tokens = 0
rerank_sync = false
rerank_sync_timeout_ms = 2000
event_date_proximity_boost = 0.0
content_dedup = "prefix"
vector_fill_missing = false
```

Te klucze `[search]`, klucze `memory_search` w [`[mcp]`](#mcp) oraz klucze `temporal_grounding` i `prompt_variant` w [`[knowledge_extraction]`](#knowledge_extraction) to dźwignie jakości. Wszystkie są domyślnie wyłączone lub neutralne: przy wartościach z dostarczanej konfiguracji wynik jest identyczny bajt po bajcie z wynikiem sprzed wprowadzenia klucza. Wartość domyślna zmienia się dopiero po zmierzonym teście A/B. Wartość odrzucona przez walidację zatrzymuje wczytanie konfiguracji.

| Klucz | Typ | Domyślnie | Opis |
|----|----|----|----|
| `rerank_date_prefix` | bool | `false` | Poprzedza każdy dokument wysyłany do rerankera tym samym znacznikiem daty, który pokazują wiersze `memory_search` (`[event:YYYY-MM-DD]`, `[observed:…]` lub `[recorded:…]`, w przeciwnym razie wyodrębniona data zdarzenia, a w ostatniej kolejności znacznik czasu wyniku). Kosztuje jeden odczyt chunka na każdego kandydata do rerankingu. Przy rerankerze LLM prefiks wlicza się do `rerank_llm_max_chars`. `false`: reranker widzi surową treść |
| `rerank_llm_max_chars` | int | `200` | Liczba znaków (nie bajtów) każdego dokumentu wysyłanego do zapasowego rerankera LLM, używanego, gdy nie jest wczytany cross-encoder ONNX. Cross-encoder ONNX ją ignoruje. Musi być dodatnia. `200` to wartość wcześniej zapisana na stałe w kodzie |
| `rerank_onnx_max_tokens` | int | `0` | Limit tokenów dla każdej pary (zapytanie, dokument) ocenianej przez cross-encoder ONNX, łącznie z tokenami specjalnymi. Tokeny są usuwane najpierw z dłuższej sekwencji (zwykle dokumentu); gdy obie mają tę samą długość, przycinane są obie. `0`: bez przycinania, a para przekraczająca limit modelu powoduje błąd asynchronicznego rerankingu, jak dotąd. Każda inna wartość musi wynosić **co najmniej 16**; mniejsze wartości odrzuca walidacja. Ignorowany, z ostrzeżeniem w logu, gdy `tokenizer.json` modelu sam ustawia przycinanie |
| `rerank_sync` | bool | `false` | Uruchamia reranker na ścieżce żądania i zwraca jego kolejność w tej samej odpowiedzi. `false`: reranking działa w tle, jego kolejność trafia do pamięci podręcznej dla następnego identycznego zapytania, a bieżąca odpowiedź zachowuje kolejność po fuzji. Działa tylko przy `rerank_enabled = true` i gdy budżet kosztów dopuszcza reranker. Przy `true` rerankowane są także żądania, których nie da się zapisać w pamięci podręcznej. Przy błędzie modelu lub przekroczeniu czasu odpowiedź zachowuje kolejność po fuzji. Wynik wypchnięty przez reranker z top-k jest śledzony z powodem odrzucenia `reranked` (zobacz [dokumentację API](api-reference.md#post-v1search)). Przed włączeniem przeczytaj ostrzeżenie pod tabelą |
| `rerank_sync_timeout_ms` | int | `2000` | Budżet czasu w milisekundach wyłącznie na wywołanie modelu w synchronicznym rerankingu. Budowanie dokumentów, oczekiwanie na blokadę pamięci podręcznej i odczyty magazynu po oczekiwaniu dochodzą do tego czasu. Musi być dodatni. Nie działa bez `rerank_sync = true` |
| `event_date_proximity_boost` | f64 | `0.0` | Waga mnożnika wyniku dla trafień, których data zdarzenia leży w zakresie dat żądania lub blisko niego (`date_from`/`date_to` albo data odczytana z treści zapytania). W zakresie, łącznie z krańcami, wynik jest mnożony przez `1 + weight`. Poza nim wzmocnienie maleje liniowo co pełny dzień UTC i osiąga `1.0` po `n + 1` dniach od zakresu, gdzie `n` to długość zakresu w dniach, ograniczona do 365. Trafienia bez daty zdarzenia i żądania bez zakresu pozostają bez zmian. Musi być skończona i nieujemna; `0.0` wyłącza wzmocnienie. Filtr dat nadal działa później, więc spadek poza zakresem zmienia tylko kolejność trafień, które filtr zachowuje (starszych rekordów, których nie odrzuca według daty). Zapytania czasowe są po ocenie ponownie sortowane według znacznika czasu, więc dla nich wzmocnienie zmienia wyniki punktowe, ale nie kolejność |
| `content_dedup` | string | `"prefix"` | Sposób łączenia niemal identycznych wyników. `"prefix"` (dotychczasowe działanie): porównuje pierwsze 150 znaków każdego wyniku po zamianie na małe litery i usuwa wynik niżej w rankingu, gdy oba mają wspólny początek dłuższy niż 80% krótszego prefiksu, a oba prefiksy mają co najmniej 30 bajtów. Wspólny początek jest liczony w znakach, ale 80% liczy się od długości w bajtach, więc tekst z wieloma znakami wielobajtowymi (np. polskimi literami) jest łączony rzadziej, a dwa identyczne prefiksy mogą oba zostać zachowane. Fragmenty różnych źródeł oraz fragment i jego własne źródło nigdy nie są łączone. `"exact"`: łączy tylko identyczne bajt po bajcie zwykłe chunki o długości co najmniej 30 bajtów i nigdy fragmenty, więc zaprzeczenie lub aktualizacja po wspólnym początku przetrwa. Każda inna wartość powoduje błąd wczytania konfiguracji. W śledzeniu wyniki usunięte w obu trybach zachowują powód odrzucenia `content_prefix_dedup` |
| `vector_fill_missing` | bool | `false` | Kandydat po fuzji, który trafił do puli przez tor inny niż wektorowy (BM25 albo tor rzadkich terminów), ale nie należy do trafień toru wektorowego, dostaje swój rzeczywisty cosinus z zapytaniem zamiast wyniku wektorowego 0. Tor wektorowy zachowuje tylko najwyższe cosinusy (`2 × max(top_k, top_k żądania)` na głównej ścieżce wyszukiwania), więc dopasowanie leksykalne tuż pod tym progiem było dotąd oceniane tylko przez BM25 i spadało na koniec puli kandydatów. Uzupełnienie używa tego samego embeddingu zapytania i tej samej listy embeddingów co tor wektorowy, stosuje `vector_similarity_floor` tak jak dla trafień toru, dzieli przez najwyższy cosinus toru i przycina wynik do [0, 1]. Kandydat bez embeddingu oraz żądanie z pustym torem wektorowym zachowują 0; zapytania klasy Simple pomijają tor wektorowy, więc nic nie jest uzupełniane. Ograniczenia: zmienia ocenę tylko kandydatów, którzy już są w puli, więc fakt spoza wszystkich torów nadal do niej nie trafi; podnosi zarówno aktualizacje, jak i nieaktualne fakty, więc przed włączeniem wymaga testu A/B; przy `decay_match_relief` powyżej 0 (w dostarczanej konfiguracji `0.70`) uzupełniony wynik wektorowy zasila też łagodzenie zaniku za jakość dopasowania, więc uzupełnieni starsi kandydaci tracą również część kary za wiek; kosztuje jedno przejście po liście embeddingów w pamięci i jeden cosinus na uzupełnionego kandydata, bez odczytu z magazynu. Trafienia wyłącznie wektorowe nie dostają uzupełnienia BM25. Trafienia BM25 zapytań składowych (`multi_query_enabled`) dołączają do toru BM25 przed fuzją i są uzupełniane; kandydaci z grafu, z wektorowego wielu zapytań i z toru operacji, dodawani po fuzji, nie są uzupełniani. Działa w `/v1/search` (oraz w MCP `memory_search`) i w wyszukiwaniu zgodności wersji hostowanej; ścieżka ambient nie jest uzupełniana. `false`: nic nie jest obliczane |

> **Ostrzeżenie: `rerank_sync`.** Nie włączaj tego klucza, dopóki nie powstanie limit równoczesnych synchronicznych rerankingów (planowana kontynuacja).
>
> - Po przekroczeniu czasu ocena ONNX nie jest anulowana: działa dalej w puli wątków blokujących, a jej wynik jest odrzucany. Przy `rerank_candidates = 20` i ~97 ms na parę (~1,94 s na wyszukiwanie) równoczesne wyszukiwania mogą gromadzić takie osierocone zadania.
> - `rerank_sync_timeout_ms` obejmuje tylko wywołanie modelu, a nie pracę wokół niego (zobacz tabelę).
> - Przy rerankerze LLM tryb synchroniczny rerankuje także żądania, których nie da się zapisać w pamięci podręcznej, a tych wydatków na LLM nie liczy licznik kosztów.

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
provider = "openai"                  # generowanie tekstu: wpis [llm.providers.<name>] lub wbudowany "openai"
api_key_env = "OPENAI_API_KEY"       # starsze pole klucza: embeddingi OpenAI + zapas dla wbudowanego dostawcy
embedding_provider = "local"         # "local" (ONNX na urządzeniu) lub "openai"
embedding_model = "text-embedding-3-small"   # używany, gdy embedding_provider = "openai"
# embedding_model_path = "/abs/path"  # katalog modelu lokalnego; domyślnie ~/.loomem/models/multilingual-e5-small
embedding_dim = 384                  # MUSI odpowiadać aktywnemu modelowi embeddingów
compression_model = "gpt-6-luna"     # model czatu dla każdego etapu opartego na LLM
timeout_secs = 10                    # wspólny klient HTTP (embeddingi OpenAI); wywołania czatu mają własny limit czasu
fallback_to_regex = true

[llm.providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
max_completion_tokens = true
reasoning_effort = "low"
omit_temperature = true
completion_tokens_floor = 2000  # tokeny rozumowania mieszczą się w małych limitach na wywołanie (test dymny, 2026-11-22)
```

| Klucz | Typ | Opis |
|----|----|----|
| `provider` | string | Dostawca generowania tekstu dla każdego etapu opartego na LLM: nazwa wpisu [`[llm.providers.<name>]`](#llmprovidersname) lub wbudowany `"openai"` (`https://api.openai.com/v1`), gdy wpis o tej nazwie nie istnieje |
| `embedding_provider` | string | `"local"` (ONNX na urządzeniu, bez klucza API) lub `"openai"`. Brak w starszej konfiguracji → traktowany jako `"openai"` |
| `api_key_env` | string | Starsze pole klucza: używane przez `embedding_provider = "openai"` i przez wbudowanego dostawcę `"openai"`. Wpis `[llm.providers.openai]` nigdy go nie czyta; jego jedynym zapasem jest wpisany wprost `api_key` |
| `embedding_model` | string | Nazwa modelu embeddingów przy `embedding_provider = "openai"` |
| `embedding_model_path` | string? | Katalog z `model.onnx` i `tokenizer.json` dla lokalnych embeddingów. Brak ustawienia → `~/.loomem/models/multilingual-e5-small` |
| `embedding_dim` | int | Wymiar wektora embeddingu — **musi odpowiadać aktywnemu modelowi** |
| `compression_model` | string | Model czatu dla każdego etapu opartego na LLM (konsolidacja, ekstrakcja, dream, …); klucze `model` poszczególnych funkcji dziedziczą go, gdy nie są ustawione |
| `timeout_secs` | int | Limit czasu wspólnego klienta HTTP (embeddingi OpenAI); każde wywołanie czatu ma własny limit czasu na żądanie |
| `fallback_to_regex` | bool | Tylko konsolidacja: kompresja regex po błędzie wywołania API generowania tekstu (nie obejmuje braku klucza) |

<span id="local-embeddings-default"></span>

### Lokalne embeddingi (domyślnie)

Nowe instalacje korzystają z **`embedding_provider = "local"`**: embeddingi są obliczane na urządzeniu przez model ONNX bez kwantyzacji (domyślnie `multilingual-e5-small`, 384 wymiary, dobra skuteczność wyszukiwania wielojęzycznego i polskiego) przy użyciu środowiska `tract` napisanego w Rust. Klucz API nie jest wymagany, a tekst nie opuszcza urządzenia. Obsługa modelu jest w domyślnej kompilacji; pliki modelu pobierzesz poleceniem:

``` sh
./scripts/fetch-embedding-model.sh         # → ~/.loomem/models/multilingual-e5-small
```

LLM generujący tekst (konsolidacja, ekstrakcja faktów, dream) działa niezależnie. Przy domyślnej konfiguracji (te podsystemy są włączone) i bez klucza aktywnego dostawcy (`OPENAI_API_KEY` w dostarczanej konfiguracji) serwer nie uruchomi się, o ile nie ustawisz `LOOMEM_ALLOW_KEYLESS=1`. Wtedy `memory_ingest` może zapisać tekst źródłowy, ale zwraca błąd braku klucza i nie wyodrębnia faktów. Ekstrakcja encji przez LLM jest wyłączona, a dream i konsolidacja kończą się błędem; `memory_store` i wyszukiwanie semantyczne działają całkowicie lokalnie. Wyłączenie tych podsystemów w `config.toml` usuwa kontrolę przy starcie, a przy `knowledge_extraction.enabled = false` `memory_ingest` zapisuje surową transkrypcję. Aby używać lokalnego LLM do generowania tekstu, skieruj `provider` do punktu końcowego zgodnego z OpenAI zadeklarowanego w [`[llm.providers.<name>]`](#llmprovidersname).

<span id="embedding-dimension-re-embedding"></span>

### Wymiar embeddingów i ponowne obliczanie

`embedding_dim` **musi** odpowiadać aktywnemu modelowi embeddingów (`multilingual-e5-small` = 384, OpenAI `text-embedding-3-small` = 1536). Wymiar użyty przy tworzeniu bazy jest zapisany w jej metadanych; w razie niezgodności serwer **odmawia uruchomienia**, zamiast po cichu mieszać wymiary wektorów. Aby zmienić dostawcę lub model istniejącej bazy, przelicz embeddingi:

``` sh
loomem-server --reembed   # przelicz wektory skonfigurowanym dostawcą (uruchom po zatrzymaniu serwera)
```

Aby korzystać z API zamiast lokalnych embeddingów, ustaw `embedding_provider = "openai"` i `embedding_dim = 1536`, wyeksportuj `OPENAI_API_KEY` i przelicz embeddingi.

<span id="llmprovidersname"></span>

### \[llm.providers.\<name\>\]

Nazwani dostawcy generowania tekstu zgodni z OpenAI. `provider` wybiera jednego po nazwie; embeddingi nie są objęte tym ustawieniem (kierują się `embedding_provider`). Wpis o nazwie `openai` doprecyzowuje wbudowanego dostawcę: jego `api_key_env` ma pierwszeństwo, a jedynym zapasem jest wpisany wprost `llm.api_key` — starsze `llm.api_key_env` nie jest dla tego wpisu czytane, więc wpis z własną, nieustawioną zmienną odmawia uruchomienia zamiast wysłać klucz OpenAI gdzie indziej. Konfiguracja bez tego wpisu (każda instalacja utworzona przed przejściem na `gpt-6-luna`) zachowuje wbudowanego dostawcę i wysyła dokładnie te same żądania co wcześniej.

``` toml
[llm.providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
max_completion_tokens = true
reasoning_effort = "low"
omit_temperature = true
completion_tokens_floor = 2000  # tokeny rozumowania mieszczą się w małych limitach na wywołanie (test dymny, 2026-11-22)

# [llm.providers.ollama]
# base_url = "http://localhost:11434/v1"   # bez api_key_env: żądania idą bez klucza
```

| Klucz | Typ | Domyślnie | Opis |
|----|----|----|----|
| `base_url` | string | wymagany | Adres główny API, np. `https://api.openai.com/v1` lub `http://localhost:11434/v1`. Bezwzględny adres `http(s)`; bez zapytania, fragmentu i danych logowania |
| `api_key_env` | string? | — | Zmienna środowiskowa z kluczem. Pominięta → bez klucza: żądania nie mają nagłówka `Authorization` (Ollama, vLLM, brama) |
| `omit_temperature` | bool | false | Usuwa `temperature` z każdego żądania czatu |
| `max_completion_tokens` | bool | false | Wysyła limit odpowiedzi jako `max_completion_tokens` zamiast wycofywanego `max_tokens` |
| `completion_tokens_floor` | int | 0 | Podnosi do tej wartości każdy limit odpowiedzi na wywołanie, który jest od niej niższy (`0` = bez zmian). Dostarczany wpis `openai` ustawia `2000`: przy `reasoning_effort = "low"` partia NER (limit 1000) i reranking LLM (limit 100) kończyły się `finish_reason: "length"` z pustą treścią, a z progiem 2000 oba wywołania się powiodły (test dymny, 2026-11-22). Limit to górna granica, nie wydatek; ekstrakcja wiedzy ma własny limit powyżej progu, więc próg jej nie dotyczy |
| `reasoning_effort` | string? | — | Jedna z wartości `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`; dodawana do każdego żądania czatu, gdy jest ustawiona. Lista to suma wartości opisanych w dokumentacji Chat Completions, a dany model może odrzucić część z nich kodem 400 — `gpt-6-luna` przyjmuje `none`, `low`, `medium`, `high`, `xhigh`, a `minimal` odrzuca (odpowiedź API, 2026-11-22). Kontrola konfiguracji odrzuca tylko wartości spoza listy; ostatnie słowo należy do modelu |

Wpis bez żadnej z czterech opcji żądania wysyła dokładnie to samo żądanie co wbudowany dostawca.

Dla `gpt-6-luna` opcje `omit_temperature` i `max_completion_tokens` z dostarczanego wpisu są wymagane, a nie opcjonalne: API odrzuca kodem 400 zarówno `temperature` inne niż 1, jak i `max_tokens` (test dymny, 2026-11-22).

Kontrola przy starcie i pole `llm_key_present` w `/health` kierują się aktywnym dostawcą: z dostarczaną konfiguracją serwer odmawia uruchomienia bez `OPENAI_API_KEY`, gdy włączone są `entity_extraction`, `knowledge_extraction` lub `dream` (chyba że ustawiono `LOOMEM_ALLOW_KEYLESS=1`), a komunikat błędu podaje zmienną środowiskową aktywnego dostawcy. Dostawca bez `api_key_env` (lokalna Ollama) jest traktowany jak obecny klucz i uruchamia się bez `LOOMEM_ALLOW_KEYLESS`.

**Istniejące instalacje.** Instalator nigdy nie nadpisuje `config.toml`, więc nic się nie zmienia, dopóki go nie edytujesz. Aby przejść na nowy domyślny model: ustaw `compression_model = "gpt-6-luna"`, usuń wszystkie wiersze `model = "gpt-4.1-mini"` w sekcjach poszczególnych funkcji i dodaj powyższą tabelę `[llm.providers.openai]` (gdy już istnieje, jej `api_key_env` ma pierwszeństwo przed wpisanym wprost `llm.api_key`, który pozostaje zapasem). Aby zostać przy starym modelu: nic nie trzeba robić przed 2027-01-14, kiedy OpenAI wycofuje `gpt-4.1-mini`.

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

<span id="mcp"></span>

## \[mcp\]

``` toml
[mcp]
search_default_top_k = 5
search_max_top_k = 20
aggregation_default_top_k = 30
aggregation_max_top_k = 30
search_include_predecessor = false
predecessor_max_chars = 400
search_group_by = "none"
```

Limity `top_k` i postać tekstu wyników narzędzia MCP `memory_search`. Trzy ostatnie klucze to dźwignie jakości (zobacz [Dźwignie jakości](#quality-levers)). Żaden z nich nie dotyczy zapytań agregujących (wyliczeniowych), których wynik pozostaje płaską listą numerowaną.

| Klucz | Typ | Domyślnie | Opis |
|----|----|----|----|
| `search_default_top_k` | int | `5` | `top_k`, gdy zwykłe zapytanie go nie podaje |
| `search_max_top_k` | int | `20` | Górny limit `top_k` dla zwykłych zapytań |
| `aggregation_default_top_k` | int | `30` | `top_k`, gdy zapytanie agregujące go nie podaje |
| `aggregation_max_top_k` | int | `30` | Górny limit `top_k` dla zapytań agregujących |
| `search_include_predecessor` | bool | `false` | Pod trafieniem oznaczonym `[UPDATED]` dodaje jeden wiersz `↳ previously [<data>]: <tekst>` z wersją, którą to trafienie bezpośrednio zastąpiło (`supersedes_id`, jeden krok). Wiersz poprzednika **nie** podlega `time_filter` ani `valid_at`: jest pokazywany także wtedy, gdy jego własna data leży poza żądanym zakresem. Jest pomijany, gdy poprzednik jest usunięty, należy do innego strumienia, nie istnieje lub sam jest jednym z trafień. Jego tekst wlicza się do `max_context_tokens` wywołania |
| `predecessor_max_chars` | int | `400` | Limit znaków tekstu poprzednika; dłuższy tekst jest przycinany i kończy się `…`. Musi być dodatni. Nie działa bez `search_include_predecessor = true` |
| `search_group_by` | string | `"none"` | `"none"`, `"session"` lub `"day"`. Wstawia nagłówek `## <klucz>` przed każdym trafieniem, które zaczyna nową grupę. Kolejność i numeracja trafień się nie zmieniają, więc grupa, która nie jest ciągła, dostaje nagłówek ponownie. `"session"`: kluczem jest `session: <id>`, gdzie id to identyfikator źródła dla fragmentu, w przeciwnym razie sesja źródłowa trafienia, a w ostatniej kolejności trafienie jest grupowane według dnia. `"day"`: `YYYY-MM-DD` ze znacznika daty trafienia albo `unknown date`. Etykieta nagłówka jest sprowadzana do jednego wiersza i ograniczona do 80 znaków (dalej `…`). Nagłówki nie wliczają się do `max_context_tokens`: budżet przycina trafienia przed renderowaniem i liczy tylko tekst trafień i poprzedników. Każda inna wartość powoduje błąd wczytania konfiguracji |

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
# model = "gpt-6-luna"  # brak: dziedziczy compression_model z [llm]
max_transcript_tokens = 20000
dedup_cosine_threshold = 0.92
contradiction_check = true
contradiction_cosine_min = 0.5
max_facts_per_chunk = 20
max_tokens = 16384
input_chunk_tokens = 4000
split_on_truncation = true
tolerate_trailing_json = false
temporal_grounding = "off"
prompt_variant = "default"
```

Steruje procesem LLM `memory_ingest`.

| Klucz | Typ | Domyślnie | Opis |
|----|----|----|----|
| `max_facts_per_chunk` | int | 20 | Limit przyjętych faktów na fragment transkrypcji (jeden zaplanowany fragment wejścia wraz z jego ponowieniami po podziale), stosowany po filtrze pewności; reguła promptu „Maximum N facts per conversation” niesie tę samą liczbę w każdym wywołaniu. Każdy fragment jest przetwarzany: nadwyżka jednego fragmentu jest liczona w `discarded_facts` i nigdy nie zatrzymuje kolejnych. Zastępuje `max_facts_per_transcript`, limit na całą transkrypcję, który po osiągnięciu zatrzymywał przetwarzanie wejścia, więc długa transkrypcja traciła wszystko poza pierwszymi 20 faktami. Stary klucz jest nadal przyjmowany jako alias; konfiguracja z oboma kluczami jest odrzucana |
| `max_tokens` | int | 16384 | Limit odpowiedzi na wywołanie ekstrakcji, wysyłany przez wpis `openai` jako `max_completion_tokens` (tokeny rozumowania mieszczą się w nim). Odpowiedź, która go osiągnie (`finish_reason: "length"`), jest odrzucana; nigdy nie jest przyjmowana częściowo |
| `input_chunk_tokens` | int | 4000 | Limit wejścia dla jednego wywołania ekstrakcji (4 znaki ≈ 1 token): transkrypcja jest dzielona na granicach wierszy na fragmenty o maksymalnie `min(input_chunk_tokens, max_transcript_tokens)` tokenach, po jednym wywołaniu na każdy. `0` = jedno wywołanie na transkrypcję do `max_transcript_tokens` (poprzedni proces), a powyżej tej wielkości nakładające się okna |
| `split_on_truncation` | bool | true | Fragment, którego odpowiedź została obcięta, jest ponownie przetwarzany jako dwie połowy rozdzielone na granicy wiersza (głębokość 2, maksymalnie 4 dodatkowe wywołania na fragment). Przy `false` obcięty fragment jest odrzucany i zgłaszany bez ponawiania |
| `tolerate_trailing_json` | bool | false | `true` przyjmuje pierwszy kompletny obiekt `{"facts": […]}` odpowiedzi, gdy jej reszta po przycięciu białych znaków to najwyżej 8 znaków `}` i `]` (odpowiedź z `finish_reason: "stop"` i zbędnym `]}` po poprawnym obiekcie traci inaczej wszystkie fakty fragmentu); odrzucony ogon jest logowany tylko jako długość, a każdy inny błąd parsowania nadal odrzuca fragment. `false` = ścisłe parsowanie, bajtowo identyczne |

Te wartości są domyślne w dostarczanej konfiguracji od 2026-11-22 (decyzja właściciela, celowo bez testu A/B — pomiarem jest pilotaż walidacyjny). Wcześniej ekstrakcja wykonywała jedno wywołanie na transkrypcję do 20 tys. tokenów, z limitem odpowiedzi 2000 tokenów i 20 faktami na transkrypcję. Wartości domyślne w kodzie dla konfiguracji pomijającej te klucze pozostają przy poprzednich (`20` / `2000` / `0` / `false`), więc starsza konfiguracja zachowuje się jak dotąd; przy `input_chunk_tokens = 0` i `max_facts_per_transcript = 20` cała transkrypcja jest jednym fragmentem, a limit na fragment jest dawnym limitem, fakt po fakcie. Jedyna widoczna zmiana dla takiej konfiguracji: transkrypcja dłuższa niż `max_transcript_tokens`, którą poprzedni proces przetwarza jako nakładające się okna, dostaje teraz limit na każde okno. Obcięta odpowiedź nie jest przyjmowana w żadnym trybie; odpowiedź zakończona `finish_reason = "length"` jest jawnym błędem na każdej ścieżce czatu, nie tylko w ekstrakcji.

`temporal_grounding` i `prompt_variant` to dźwignie jakości (zobacz [Dźwignie jakości](#quality-levers)):

| Klucz | Typ | Domyślnie | Opis |
|----|----|----|----|
| `temporal_grounding` | string | `"off"` | Odrzuca wyodrębnione fakty, których czas względny (np. „three weeks ago” lub „yesterday”) albo `event_date_context` nie występuje we fragmencie źródłowym. `"off"`: bez kontroli; prompt i przyjęte fakty są identyczne bajt po bajcie z wcześniejszymi. `"filter"`: odrzuca takie fakty; prompt ekstrakcji się nie zmienia. `"prompt_and_filter"`: jak `"filter"`, a do promptu systemowego ekstrakcji dodawane jest na początku powiadomienie TEMPORAL GROUNDING, co zmienia prompt wysyłany do LLM. Każda inna wartość powoduje błąd wczytania konfiguracji |
| `prompt_variant` | string | `"default"` | Prompt systemowy ekstrakcji. `"default"`: prompt jest identyczny bajt po bajcie z wcześniejszym. `"relations"`: do promptu systemowego ekstrakcji dodawane jest na początku powiadomienie RELATIONS AND VALUES, które prosi model, by zachował podaną przyczynę w fakcie, który ona wyjaśnia, kierunek zmiany (z czego na co, wzrost lub spadek, rozpoczęcie lub zakończenie; nigdy odwrócony) oraz liczby, kwoty, waluty i daty razem z encją, do której należą. Zmienia to prompt wysyłany do LLM; format odpowiedzi i parser się nie zmieniają. `"relations_v2"`: powiadomienie `"relations"` uzupełnione o dwie klauzule: SCOPE (warunek podany w następnym zdaniu zostaje w fakcie, który ogranicza, z nazwanym odniesieniem zamiast zaimka) oraz SPEAKER (wymieniona z nazwiska osoba inna niż użytkownik i asystent, która przed czymś ostrzegła, coś doradziła, zasugerowała lub zaproponowała, zostaje w fakcie wymieniona jako ta, która to zrobiła, razem z tym, co powiedziała; `attributed_to` się nie zmienia). Tylko do ewaluacji: wymaga A/B na zbiorze holdout przed jakąkolwiek zmianą domyślnej wartości. Przy `temporal_grounding = "prompt_and_filter"` powiadomienie TEMPORAL GROUNDING poprzedza powiadomienie RELATIONS AND VALUES, a oba następują po powiadomieniu bezpieczeństwa (dane niezaufane) na początku promptu. Każda inna wartość powoduje błąd wczytania konfiguracji |

Kontrola `temporal_grounding` porównuje tekst po normalizacji białych znaków i zamianie na małe litery i obejmuje tylko część wyrażeń względnych: fakt, który używa innej formy, przechodzi kontrolę treści bez sprawdzenia. Znane formy, których nie obejmuje:

- angielskie: „last night”, „this morning”, „next week”, „in two weeks”, „last Friday/weekend/summer”, „a couple of weeks ago”, „a few/several days ago” oraz liczebniki słowne powyżej dziesięciu („eleven days ago”);
- polskie: „dziś”, „przedwczoraj”, „w zeszłym tygodniu”, „tydzień/miesiąc/rok temu” oraz liczebniki słowne powyżej pięciu („sześć dni temu” i wyższe).

Kompilacja z funkcją cargo `evaluation` podaje liczbę faktów odrzuconych przez kontrolę jako `grounding_rejected_facts` w potwierdzeniu ekstrakcji `memory_ingest` (`extraction_receipt.outcome`). Pole jest pomijane, gdy kontrola jest wyłączona, a zwykłe kompilacje nie zawierają potwierdzenia.

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
# model = "gpt-6-luna"  # brak: dziedziczy compression_model z [llm]
batch_size = 10
flush_interval_secs = 3
queue_capacity = 200
confidence_threshold = 0.7
max_tokens_per_batch = 2000
```

Asynchroniczne rozpoznawanie encji przez LLM dla encji spoza słownika. `batch_size` wynosi 10 od 2026-11-22 (było 20): przy `reasoning_effort = "low"` partia 14–15 fragmentów zużyła 1696 z 2000 tokenów odpowiedzi, a 10 zostawia zapas na tokeny rozumowania, które mieszczą się w tym samym limicie.

------------------------------------------------------------------------

<span id="contradiction"></span>

## \[contradiction\]

``` toml
[contradiction]
enabled = true
similarity_threshold = 0.70
max_candidates = 5
# model = "gpt-6-luna"  # brak: dziedziczy compression_model z [llm]
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
# model = "gpt-6-luna"  # brak: dziedziczy compression_model z [llm]
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
# model = "gpt-6-luna"  # brak: dziedziczy compression_model z [llm]
cost_cap_usd_per_run = 0.10
auto_trigger_threshold = 50
auto_cooldown_secs = 900
schedule_enabled = true
schedule_interval_secs = 86400
schedule_timeout_secs = 3600
```

| Klucz | Typ | Opis |
|----|----|----|
| `batch_size` | int | Liczba fragmentów na przebieg dream |
| `min_group_size` | int | Minimalna liczba fragmentów tematu do połączenia |
| `cost_cap_usd_per_run` | f64 | Limit bezpieczeństwa na sesję dream |
| `auto_trigger_threshold` | int | Uruchom dream po tylu nowych chunkach w strumieniu prywatnym (`0` = wyłączone; domyślnie w kodzie `0`, dostarczany `config.toml` ustawia `50`) |
| `auto_cooldown_secs` | int | Minimalna liczba sekund między automatycznymi uruchomieniami w tym samym strumieniu (domyślnie `900`) |
| `schedule_enabled` | bool | Uruchamiaj cykliczny przebieg dream dla każdego aktywnego strumienia. Domyślnie w kodzie `false` (konfiguracja sprzed tego klucza pozostaje na żądanie); dostarczany `config.toml` ustawia `true` |
| `schedule_interval_secs` | int | Liczba sekund między zaplanowanymi przebiegami (domyślnie `86400`, czyli doba; dozwolony zakres `3600` do `2592000`) |
| `schedule_timeout_secs` | int | Miękki termin dla całego zaplanowanego przebiegu: po tylu sekundach nie startuje żaden nowy strumień, a strumień w toku kończy się normalnie, a następny przebieg wznawia od pierwszego strumienia odciętego przez termin (w porządku posortowanym, z zawinięciem), więc późniejsze strumienie nie są pomijane w nieskończoność (domyślnie `3600`, minimum `60`) |

Zaplanowany przebieg uruchamia dream dla każdego aktywnego strumienia, także współdzielonych, i wymaga jednocześnie `[dream] enabled = true` oraz `[scheduler] enabled = true`. Harmonogram jest ustawieniem całej instancji jednoużytkownikowego silnika, więc jeden przebieg obejmuje wszystkie strumienie. Strona Ustawienia w dashboardzie (lub `PUT /v1/dream/schedule`) może nadpisać `enabled` i interwał w trakcie działania; nadpisanie jest zapisywane w bazie silnika i przetrwa restart, a `schedule_enabled` i `schedule_interval_secs` pozostają wartościami zapasowymi, gdy nic nie nadpisano. Włączenie harmonogramu po jego wyłączeniu ustawia następny przebieg o jeden interwał od tej chwili, bez nadrabiania zaległego przebiegu. Restart nie ustawia harmonogramu od nowa: przebieg, który stał się wymagalny, gdy silnik był zatrzymany, uruchamia się raz przy pierwszym sprawdzeniu po starcie (co najwyżej jedno nadrobienie, potem normalny interwał). Wszystkie punkty wejścia dream (harmonogram, automatyczne uruchomienie, `memory_dream`, `POST /v1/dream`) wzajemnie się wykluczają w obrębie magazynu i strumienia: uruchomienie na strumieniu, który już jest przetwarzany, jest natychmiast pomijane. `POST /v1/dream` zwraca `skipped_busy: true`, `memory_dream` odpowiada `Dream skipped: another dream run is in progress on this stream.`, zaplanowany przebieg liczy takie strumienie w `last_result.streams_skipped_busy`, a automatyczne uruchomienie tylko zapisuje pominięcie w logu. Ręczne `memory_dream` / `POST /v1/dream` i automatyczne uruchomienie po progu chunków działają niezależnie od harmonogramu.

------------------------------------------------------------------------

<span id="memory_generator"></span>

## \[memory_generator\]

``` toml
[memory_generator]
enabled = true
max_chunks = 200
max_sections = 20
# model = "gpt-6-luna"  # brak: dziedziczy compression_model z [llm]
```

Steruje wynikiem punktu końcowego `generate-memory-md`.

------------------------------------------------------------------------

<span id="environment-variables"></span>

## Zmienne środowiskowe

| Zmienna | Przeznaczenie | Wymagana |
|----|----|----|
| `LOOMEM_AUTH_TOKEN` | Klucz Bearer API (nazwa konfigurowana przez `server.auth_token_env`) | Nie (bez ustawienia uwierzytelnianie wyłączone — lokalny tryb passthrough) |
| `OPENAI_API_KEY` | Klucz API LLM (nazwa konfigurowana przez `llm.api_key_env`, a dla dostawcy przez `llm.providers.<name>.api_key_env`) | Tylko dla dostawcy generowania tekstu, którego wpis ją wskazuje (dostarczany `[llm.providers.openai]` tak robi), lub embeddingów OpenAI (`embedding_provider = "openai"`). Przy lokalnych embeddingach i bez klucza domyślna konfiguracja uruchamia się tylko z `LOOMEM_ALLOW_KEYLESS=1`, a etapy LLM są niedostępne (`memory_store` i wyszukiwanie nadal działają). |
| `LOOMEM_CONFIG` | Ścieżka do pliku konfiguracji | Nie (domyślnie: `config.toml`) |
| `PORT` | Nadpisz port serwera | Nie |
| `SERVER_ORIGIN` | Bazowy URL przekierowania OAuth | Nie (dla MCP Remote) |
| `LOOMEM_LOG_FORMAT` | `"json"` dla dzienników strukturalnych | Nie |
| `LOOMEM_AT_REST_MASTER_KEY` | Klucz główny włączający szyfrowanie danych w spoczynku (32 bajty w base64) | Nie |
| `LOOMEM_AT_REST_EXPECT_ENABLED` | Po ustawieniu odmów uruchomienia bez klucza głównego | Nie |
| `LOOMEM_AMBIENT_CACHE_TTL_SECS` | TTL pamięci podręcznej dla odpowiedzi `/v1/ambient` | Nie (domyślnie 60) |
| `TELEGRAM_BOT_TOKEN`, `LOOMEM_TELEGRAM_CHAT_ID` | Opcjonalny webhook alertów o kosztach | Nie |

Informacje o szyfrowaniu danych w spoczynku: [SECURITY.md — dane w spoczynku](SECURITY.md#data-at-rest).
