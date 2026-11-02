<span id="architecture"></span>

# Architektura

<span id="overview"></span>

## Przegląd

Loomem to workspace Rust z czterema pakietami:

| Pakiet | Rola |
|----|----|
| `loomem-core` | Biblioteka główna — przechowywanie, wyszukiwanie, konsolidacja, graf, LLM i embeddingi |
| `loomem-server` | Serwer HTTP i MCP (Axum), uwierzytelnianie, obsługa żądań i routing |
| `loomem-cli` | Interfejs wiersza poleceń do bezpośredniej obsługi |
| `loomem-migrate` | Narzędzia do migracji danych |

Cała logika aplikacji znajduje się w `loomem-core`. Serwer jest cienką warstwą HTTP, która kieruje żądania do funkcji biblioteki głównej.

------------------------------------------------------------------------

<span id="data-model"></span>

## Model danych

<span id="chunk"></span>

### Chunk

Podstawowa jednostka pamięci. Każda informacja — surowe zdarzenie, skompresowane podsumowanie czy grupa semantyczna — jest chunkiem.

``` text
chunk:L{level}:{uuid}  →  Chunk (JSON in RocksDB)
```

**Pola podstawowe:**

| Pole | Typ | Opis |
|----|----|----|
| `id` | UUID | Unikalny identyfikator |
| `content` | String | Tekst pamięci |
| `stream` | String | Przestrzeń nazw / granica izolacji (domyślnie: `__user_default__`) |
| `level` | 0, 1 | Poziom pamięci (surowa, skompresowana) |
| `score` | f64 | Trafność z uwzględnieniem zaniku (0.0 – 1.0) |
| `timestamp` | u64 | Czas zapisu (sekundy Unix) |
| `importance` | f64 | Wzmocnienie zależne od stopnia zaskoczenia (0.7 – 1.5) |
| `access_count` | u32 | Licznik trafień w wyszukiwaniu (dla adaptacyjnego zaniku) |
| `persistent` | bool | Wyłączony z zaniku |
| `is_latest` | bool | Najnowszy element łańcucha zastępowania |

**Pola konsolidacji:**

| Pole             | Typ           | Opis                                  |
|------------------|---------------|---------------------------------------|
| `consolidated`   | bool          | L0: został skompresowany do L1        |
| `source_ids`     | Vec\\&lt;String&gt; | L1: które chunki L0 zostały połączone |
| `prompt_version` | u32           | Wersja użytego promptu konsolidacji   |

**Pola sprzeczności i wersjonowania:**

| Pole             | Typ              | Opis                               |
|------------------|------------------|------------------------------------|
| `superseded_by`  | Option\\&lt;String&gt; | Wskazuje nowszą wersję             |
| `supersedes_id`  | Option\\&lt;String&gt; | Chunk zastąpiony przez ten element |
| `root_memory_id` | Option\\&lt;String&gt; | Korzeń łańcucha wersji             |
| `version`        | u32              | Licznik wersji semantycznej        |

**Metadane ekstrakcji wiedzy:**

| Pole | Typ | Opis |
|----|----|----|
| `extraction_meta.fact_type` | Enum | `PreferenceOrDecision`, `ProjectState`, `Fact` |
| `extraction_meta.subject` | String | Nazwa encji (osoba, projekt) |
| `extraction_meta.event_date` | String | Data zdarzenia w formacie ISO |
| `extraction_meta.confidence` | f64 | Pewność ekstrakcji LLM (0.0 – 1.0) |

<span id="memory-tiers"></span>

### Poziomy pamięci

``` text
L0 (Raw)          L1 (Compressed)
─────────         ─────────────
Verbatim input    LLM-summarized
Fast decay        Medium decay
```

**Przepływ promocji:**

``` text
Ingest → L0 (raw event)
            │
            ▼  [Consolidation worker, every 5 min]
         L1 (compressed observations)
```

Osobny proces klastrowania okresowo grupuje chunki L1 według podobieństwa embeddingów; klastry trafiają do mechanizmu skojarzeń, nie do osobnego poziomu przechowywania.

<span id="entity-graph"></span>

### Graf encji

Encje i ich relacje tworzą graf wiedzy przechowywany w RocksDB:

``` text
EntityNode {
    canonical_name: "Cursor"
    entity_type: "TECHNOLOGY"
    aliases: ["cursor", "Cursor IDE"]
    chunk_ids: ["abc-123", "def-456"]   // dowód
}

Edge {
    source: "Alice"
    target: "Cursor"
    relation_type: "uses"
    chunk_ids: ["abc-123"]              // dowód
}
```

**Izolacja według strumienia:** Każda encja i krawędź jest przypisana do `stream_id`. Ta sama nazwa encji w dwóch strumieniach otrzymuje oddzielne węzły. Indeksy nazw i aliasów mają prefiks strumienia.

**Schemat kluczy grafu w RocksDB:**

``` text
graph:entity:{id}                    → EntityNode (JSON, includes stream_id)
graph:s:{stream_id}:name:{lower()}   → entity_id
graph:s:{stream_id}:alias:{lower()}  → entity_id
graph:adj:{src_id}:{edge_id}         → target_id
graph:radj:{tgt_id}:{edge_id}        → source_id
graph:chunk:{chunk_id}               → [entity_ids]  (reverse index)
```

------------------------------------------------------------------------

<span id="storage-layer"></span>

## Warstwa przechowywania

<span id="rocksdb"></span>

### RocksDB

Podstawowy trwały magazyn. Przechowuje chunki, embeddingi, graf, użytkowników i dane o kosztach.

**Rodziny kolumn:**

| Rodzina | Zawartość |
|----|----|
| default | Chunki (`chunk:L{n}:{id}`), graf |
| embeddings | Wektory (`emb:{id}` → tablica f32) |
| cost | Dzienne liczniki kosztów (`cost:{date}`) |
| keys | Zabezpieczone klucze szyfrowania danych dla każdego strumienia (szyfrowanie danych w spoczynku) |

**Konfiguracja:**

- Kompresja: LZ4
- Bufor zapisu: 64 MB x 3 bufory
- Maksymalna liczba otwartych plików: 1000

<span id="tantivy"></span>

### Tantivy

Indeks wyszukiwania pełnotekstowego. Odzwierciedla treść chunków z dodatkowymi indeksowanymi polami.

**Schemat:**

| Pole         | Typ    | Waga | Cel                                         |
|--------------|--------|------|---------------------------------------------|
| `id`         | String | —    | Odwołanie do chunka                         |
| `content`    | Tekst  | 1.0  | Główne pole wyszukiwania                    |
| `entities`   | Tekst  | 0.2  | Wzmianki o encjach (oddzielone przecinkami) |
| `relations`  | Tekst  | 0.2  | Trójki relacji                              |
| `stream`     | String | —    | Filtrowanie                                 |
| `timestamp`  | i64    | —    | Filtrowanie dat                             |
| `event_date` | i64    | —    | Zapytania czasowe                           |
| `level`      | i64    | —    | Filtrowanie poziomu                         |

**Stemming języka polskiego** jest włączony dla pola treści.

<span id="vector-store"></span>

### Magazyn wektorowy

Embeddingi są przechowywane w rodzinie kolumn embeddings w RocksDB.

**Dostawcy:**

- **Lokalny (domyślny)** — `multilingual-e5-small` (384 wymiary) przez środowisko tract ONNX (czysty Rust, bez zależności natywnych, działa offline)
- **OpenAI** — `text-embedding-3-small` (1536 wymiarów), włączany przez `embedding_provider = "openai"`

**Kolejka embeddingów:** przetwarzanie wsadowe (50 elementów, opróżnianie co 5 s), aby ograniczyć wpływ opóźnień API.

<span id="intent-log-wal"></span>

### Dziennik zamiarów (WAL)

Dziennik zapisu z wyprzedzeniem zapewnia spójność między magazynami RocksDB i Tantivy.

``` text
1. Append: PENDING(op_type, chunk_id)
2. Write to RocksDB
3. Write to Tantivy
4. Append: COMMITTED(op_type, chunk_id)
```

Podczas odzyskiwania po awarii:

- PENDING bez COMMITTED → wycofanie częściowych zapisów
- COMMITTED → sprawdzenie danych w obu magazynach

------------------------------------------------------------------------

<span id="ingest-pipeline"></span>

## Przetwarzanie przy zapisie

Treść jest oczyszczana przed każdym zapisem:

``` text
Input content
    │
    ▼  [sanitizer.rs]
 1. HTML tag stripping + entity decoding
 2. Instruction injection detection (18 patterns — logs warning, does not block)
    │
    ▼  [pii_filter.rs]
 3. PII redaction (phones, emails, PESEL, blocklist words)
    │
    ▼  [persist_chunk]
 4. RocksDB store (sanitized content)
 5. Entity extraction + graph population (stream-scoped)
 6. Tantivy indexing
 7. Embedding queue
 8. Contradiction detection
```

Sanitizer wykrywa próby wstrzyknięcia instrukcji, ale ich nie blokuje — treść jest zapisywana po oczyszczeniu. Filtr danych osobowych zastępuje dane wrażliwe znacznikami `[PHONE]`, `[EMAIL]`, `[ID]`, `[REDACTED]` przed zapisem.

**Źródło → fragmenty → indeks (funkcja opcjonalna, `[fragments] enabled = true`, domyślnie wyłączona).** W operacji `memory_ingest` cała rozmowa jest najpierw zapisywana jako kanoniczny chunk źródłowy; etap fragmentacji odczytuje go w zapisanej postaci i dzieli na fragmenty odpowiadające wypowiedziom (po jednym na turę rozmowy; długie tury dzielone są dalej na granicach akapitów, zdań i białych znaków), mieszczące się w oknie wejściowym modelu embeddingów. Każdy fragment to chunk poziomu 0 (`{source_id}:f{index}`), zawierający rolę autora, datę rozmowy i dokładny zakres bajtów w źródle. Przechodzi przez tę samą ścieżkę `persist_chunk` co źródło (zapis w RocksDB, ekstrakcja encji, indeksowanie Tantivy, kolejka embeddingów), dzięki czemu otrzymuje własny indeks i embedding. Fragmenty są oznaczane jako `consolidated` od chwili utworzenia (nigdy nie są wejściem do konsolidacji ani dream), nie mają własnej daty zdarzenia i są oznaczane do usunięcia razem ze źródłem. Ekstrakcja faktów następuje po etapie fragmentacji i nigdy nie jest przez niego blokowana. Zobacz [Konfiguracja](configuration.md#fragments).

------------------------------------------------------------------------

<span id="search-pipeline"></span>

## Przetwarzanie zapytań

Zapytanie przechodzi przez wiele etapów:

<span id="1-query-classification"></span>

### 1. Klasyfikacja zapytania

``` text
"what do you know about me?"   → Profile
"how many projects do I run?"  → Aggregation (top_k boosted to 30)
"when did I change my IDE?"    → Temporal (date filtering)
"why did I choose Rust?"       → Complex (top_k = 20)
"my dog's name"                → Simple (top_k = 3, BM25 only)
```

<span id="2-date-filter-extraction"></span>

### 2. Wydobycie filtra dat

Odczytuje daty względne z tekstu zapytania:

- „w zeszłym tygodniu” → `date_from: now - 7d`
- „w marcu” → `date_from: 2026-03-01, date_to: 2026-03-31`
- Jawnie podane parametry `date_from`/`date_to` mają pierwszeństwo

<span id="3-parallel-search"></span>

### 3. Wyszukiwanie równoległe

Wyszukiwanie BM25 i wektorowe odbywa się równolegle:

**BM25 (Tantivy):**

``` text
QueryParser(content^1.0, entities^0.2, relations^0.2)
  + stream filter
  + date range filter
  → ranked results
```

**Wektorowe (podobieństwo kosinusowe):**

``` text
query_embedding = embed(query_text)
for each stored embedding:
    score = cosine(query_embedding, chunk_embedding)
→ top-K by similarity
```

<span id="4-fusion"></span>

### 4. Łączenie wyników

``` text
normalized_bm25   = bm25_score / max_bm25
normalized_vector  = vector_score / max_vector
fusion_score       = 0.6 * normalized_vector + 0.4 * normalized_bm25
```

<span id="5-time-decay"></span>

### 5. Zanik w czasie

``` text
age_days = (now - chunk.timestamp) / 86400
lambda   = { L0: search.decay.l0_lambda, L1: search.decay.l1_lambda }
decay    = e^(-lambda * age_days)
score    = fusion_score * decay
```

<span id="6-graph-enhancement"></span>

### 6. Rozszerzenie grafem

Dla najlepszych wyników wyszukuje powiązane encje i dodaje połączone z nimi chunki:

``` text
result "Cursor" → entity "Cursor" → edges → related entities
  → neighbor chunks added with score * boost_factor (0.3)
```

<span id="7-deduplication"></span>

### 7. Deduplikacja

Łączy niemal identyczne wyniki (o wysokim podobieństwie kosinusowym treści).

<span id="8-optional-reranking"></span>

### 8. Opcjonalna ponowna ocena

Jeżeli jest włączona, 20 najlepszych kandydatów otrzymuje nowe oceny za pomocą:

- lokalnego modelu ONNX cross-encoder (~97 ms na parę) albo
- asynchronicznej ponownej oceny LLM ze spekulacyjną pamięcią podręczną

<span id="9-implicit-boost"></span>

### 9. Niejawne wzmocnienie

Wyszukiwania inne niż próbne zwiększają `access_count` i wzmacniają `importance` zwróconych chunków (limit 1.5, przerwa jednej godziny).

<span id="10-response"></span>

### 10. Odpowiedź

``` json
{
  "results": [
    {
      "chunk_id": "abc-123",
      "content": "User prefers Cursor over VSCode",
      "score_final": 0.87,
      "trace_info": {
        "level": "L1",
        "source": "consolidation",
        "is_latest": true,
        "access_count": 5
      }
    }
  ],
  "trace_metadata": {
    "total_results_before_topk": 42,
    "search_latency_us": 1200
  }
}
```

------------------------------------------------------------------------

<span id="background-workers"></span>

## Procesy w tle

Harmonogram zarządza zadaniami w tle. Wszystkie procesy można wstrzymać i wznowić podczas działania przez `POST /admin/workers/pause|resume` (przydatne podczas ewaluacji). Stan wstrzymania to `AtomicBool` współdzielony przez `AppState` i `Scheduler` — nie jest zachowywany po restarcie.

<span id="consolidation-l0-l1"></span>

### Konsolidacja (L0 → L1)

| Ustawienie | Domyślnie |
|----|----|
| Interwał | 5 minut |
| Rozmiar partii | 200 chunków |
| Minimalny wiek | 60 sekund |
| Minimalna liczba chunków | 3 na strumień |
| Styl | `structured` (fakty z typem i rozstrzyganiem dat) |
| Próg podobieństwa | 0.20 (kosinusowy, do grupowania tematów) |

**Proces:** 1. Skanowanie nieskonsolidowanych chunków L0 2. Grupowanie według strumienia (izolacja użytkownika) 3. **Podział na podgrupy według podobieństwa tematu** — zachłanne klastrowanie embeddingów (próg kosinusowy). Zapobiega łączeniu niepowiązanych faktów w jednym chunku L1. 4. Redakcja danych osobowych (dla każdej podgrupy) 5. Kompresja przez LLM (gpt-4.1-mini) — jedno wywołanie na podgrupę 6. Utworzenie chunka L1 z `source_ids` wskazującymi na L0 7. Indeksowanie w Tantivy i kolejka embeddingów 8. Oznaczenie L0 jako `consolidated: true`

Chunki bez embeddingów trafiają do jednej grupy (zachowanie sprzed klastrowania). Strumienie o jednym temacie tworzą jedną podgrupę — bez ryzyka regresji.

<span id="decay"></span>

### Zanik

| Ustawienie      | Domyślnie        |
|-----------------|------------------|
| Interwał        | 1 godzina        |
| Współczynnik L0 | 0.990 na godzinę |
| Współczynnik L1 | 0.995 na godzinę |
| Próg uśpienia   | 0.01             |
| Adaptacyjny     | włączony (ACT-R) |

**Adaptacyjny zanik:** chunki o wysokim `access_count` zanikają wolniej (`adaptive_dampening` / `adaptive_cap` w `[worker.decay_worker]`).

<span id="clustering"></span>

### Klastrowanie

| Ustawienie                 | Domyślnie                  |
|----------------------------|----------------------------|
| Interwał                   | 6 godzin                   |
| Algorytm                   | k-means na embeddingach L1 |
| Maksymalna liczba iteracji | 1000                       |

Wyniki klastrowania trafiają do mechanizmu skojarzeń (poniżej); dla klastrów nie ma osobnego poziomu przechowywania.

<span id="entity-extraction-queue"></span>

### Kolejka ekstrakcji encji

| Ustawienie           | Domyślnie    |
|----------------------|--------------|
| Interwał opróżniania | 3 sekundy    |
| Pojemność kolejki    | 200          |
| Próg pewności        | 0.5          |
| Model                | gpt-4.1-mini |

Asynchroniczne rozpoznawanie encji przez LLM działa w tle dla encji nieobecnych w słowniku.

<span id="embedding-queue"></span>

### Kolejka embeddingów

| Ustawienie           | Domyślnie |
|----------------------|-----------|
| Rozmiar partii       | 50        |
| Interwał opróżniania | 5 sekund  |

<span id="associator-eca-serendipity-engine"></span>

### Mechanizm skojarzeń (ECA)

| Ustawienie                       | Domyślnie                                |
|----------------------------------|------------------------------------------|
| Interwał                         | 6 godzin (z klastrowaniem)               |
| Minimalny poziom nieoczywistości | 0.1                                      |
| Maksymalna liczba skojarzeń      | 3 na zapytanie                           |
| Mechanizmy                       | przejście po grafie, czasowe, sąsiedztwo |

**Komponenty:**

- **Klastrowanie** — k-means na embeddingach chunków, osobno dla każdego strumienia
- **Przejście po grafie** — losowe przejście preferujące słabe powiązania (mniej wspólnych chunków oznacza większą nowość)
- **Czasowe** — wyszukiwanie chunków z podobnego okresu
- **Ocena nieoczywistości** — trafność × (1 - oczywistość) × odległość klastrów

<span id="hard-purge-retention-worker"></span>

### Trwałe usuwanie (proces retencji)

| Ustawienie    | Domyślnie                                         |
|---------------|---------------------------------------------------|
| Interwał      | 24 godziny (`retention.hard_purge_interval_secs`) |
| Okno retencji | 30 dni (`retention.soft_delete_days`)             |

Wyszukuje chunki miękko usunięte, dla których minął okres odzyskiwania. Kolejność usuwania: odwołania w grafie → Tantivy → trwałe usunięcie z RocksDB (chunk, embedding, encje i relacje).

<span id="dream-auto-consolidation"></span>

### Dream (automatyczna konsolidacja)

| Ustawienie | Domyślnie |
|----|----|
| Automatyczne uruchomienie | 50 nowych chunków na strumień prywatny, z 15-minutową przerwą między uruchomieniami |
| Rozmiar partii | 50 chunków |
| Minimalny rozmiar grupy | 2 |
| Limit kosztu | 0,10 USD / uruchomienie |

------------------------------------------------------------------------

<span id="authentication"></span>

## Uwierzytelnianie

Loomem jest przeznaczony dla jednego użytkownika: jeden klucz API kontroluje dostęp do całej instancji.

- Klucz jest odczytywany ze zmiennej środowiskowej wskazanej przez `server.auth_token_env` w `config.toml` (domyślnie `LOOMEM_AUTH_TOKEN`).
- Wszystkie żądania wymagają `Authorization: Bearer <key>`; `/health` pozostaje dostępny bez uwierzytelnienia.
- Jeżeli nie skonfigurowano klucza, serwer działa w **lokalnym trybie bez uwierzytelniania** — każde żądanie jest przyjmowane z uprawnieniami administratora. Używaj go tylko do lokalnego developmentu.

Dane zapisane bez jawnego strumienia trafiają do domyślnego strumienia `__user_default__`. Dodatkowe strumienie (z `[streams]` / `[namespaces]` w konfiguracji) dzielą dane w ramach tej samej instancji — są granicą organizacyjną, a nie oddzielnymi tożsamościami.

<span id="oauth-20"></span>

### OAuth 2.0

Dla zdalnego konektora MCP (claude.ai):

- dynamiczna rejestracja klienta (RFC 7591)
- przepływ Authorization Code z PKCE
- użytkownik podaje klucz API podczas autoryzacji
- token dostępu = klucz API (bez dodatkowej warstwy tokenów)

------------------------------------------------------------------------

<span id="mcp-integration"></span>

## Integracja MCP

Loomem implementuje Model Context Protocol (MCP) jako punkt końcowy JSON-RPC 2.0 pod adresem `POST /mcp`.

**Przepływ żądania:**

``` text
Claude (MCP client)
    │
    ▼
POST /mcp (JSON-RPC)
    │
    ▼
mcp::handler → parse request → extract tool + args
    │
    ▼
mcp::dispatcher → match tool name → call internal handler
    │
    ▼
loomem-core / handlers → execute → return ToolResult
    │
    ▼
JSON-RPC response → Claude
```

**Zarządzanie sesjami:** Tokeny OAuth są przypisywane do sesji, a sesje do `stream_id` w celu izolacji danych.

------------------------------------------------------------------------

<span id="crash-recovery"></span>

## Odzyskiwanie po awarii

1.  **Odtworzenie dziennika zamiarów** — przy starcie skanowanie WAL w poszukiwaniu niezakończonych operacji
2.  **Wykrywanie częściowych zapisów** — sprawdzenie spójności RocksDB i Tantivy
3.  **Usuwanie osieroconych danych** — usunięcie chunków oznaczonych `in_progress` po nieudanej konsolidacji
4.  **Odbudowa Tantivy** — przy niezgodności wersji schematu odbudowa indeksu z RocksDB jako źródła prawdy

------------------------------------------------------------------------

<span id="cost-tracking"></span>

## Śledzenie kosztów

Każde wywołanie LLM (konsolidacja, ekstrakcja, embedding) jest rejestrowane:

``` text
[cost]
daily_cap_usd = 15.00           # twardy limit
alert_threshold_usd = 10.00     # ostrzeżenie
anomaly_multiplier = 3.0        # 3-krotność typowej wartości = alert anomalii
```

Koszty są zapisywane w rodzinie kolumn RocksDB. Procesy sprawdzają budżet przed każdym wywołaniem LLM.
