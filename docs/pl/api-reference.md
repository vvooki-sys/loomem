<span id="api-reference"></span>

# Dokumentacja API

Adres bazowy: `http://localhost:3030` (lub adres wdrożonej instancji)

Uwierzytelnianie: gdy zmienna środowiskowa wskazana przez `server.auth_token_env` (domyślnie `LOOMEM_AUTH_TOKEN`) zawiera niepusty klucz API, wysyłaj `Authorization: Bearer <token>` do chronionych punktów końcowych, w tym `/v1`, `/api`, `/admin` i `/mcp`. `/health`, punkty wykrywania OAuth i ścieżki `/oauth/*` oraz statyczny panel znajdują się poza warstwą Bearer; punkty OAuth stosują własne kontrole protokołu. Bez klucza chronione punkty końcowe przyjmują żądania z uprawnieniami administratora. Serwer odmawia nasłuchiwania poza interfejsem lokalnym bez klucza, chyba że jawnie ustawiono `LOOMEM_ALLOW_UNAUTH=1`.

Gdy nie podano `stream`, dane są odczytywane i zapisywane w domyślnym strumieniu `__user_default__`.

------------------------------------------------------------------------

<span id="storage"></span>

## Przechowywanie

<span id="post-v1store"></span>

### POST /v1/store

Zapisz fragment pamięci.

**Żądanie:**

``` json
{
  "content": "User prefers dark mode in all tools",
  "stream": "100",
  "level": 0,
  "metadata": { "source": "user-stated" },
  "user_id": "alice",
  "app_id": "claude"
}
```

| Pole | Typ | Wymagane | Domyślnie | Opis |
|----|----|----|----|----|
| `content` | string | tak | — | Tekst pamięci (1–1 mln znaków) |
| `stream` | string | nie | `__user_default__` | Identyfikator przestrzeni nazw |
| `level` | int | nie | 0 | Poziom pamięci (0 = surowy, 1 = skonsolidowany) |
| `metadata` | object | nie | null | Własne metadane JSON |
| `user_id` | string | nie | null | Identyfikator twórcy |
| `app_id` | string | nie | null | Identyfikator aplikacji |

**Odpowiedź:**

``` json
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "status": "stored"
}
```

**Efekty uboczne:**

- Oczyszczanie przed zapisem (usuwanie HTML, wykrywanie wstrzyknięć instrukcji)
- Redakcja danych osobowych (telefony, e-maile, PESEL, słowa z listy blokad)
- Ekstrakcja encji (słownik i kolejka LLM)
- Generowanie embeddingów (kolejka asynchroniczna)
- Indeksowanie Tantivy
- Uzupełnianie grafu (w obrębie strumienia)
- Obliczanie zaskoczenia (korekta ważności)
- Wykrywanie sprzeczności z istniejącymi wspomnieniami

Ekstrakcja wiedzy z pełnych transkrypcji rozmów za pomocą LLM jest dostępna przez narzędzie `memory_ingest` MCP — zobacz [Opis narzędzi MCP](mcp-tools.md).

------------------------------------------------------------------------

<span id="search"></span>

## Wyszukiwanie

<span id="post-v1search"></span>

### POST /v1/search

Wyszukiwanie hybrydowe we wszystkich poziomach pamięci.

**Żądanie:**

``` json
{
  "query": "What IDE does the user prefer?",
  "top_k": 5,
  "stream": "100",
  "date_from": "2026-01-01",
  "date_to": "2026-04-03",
  "trace": true,
  "dry_run": false
}
```

| Pole | Typ | Wymagane | Domyślnie | Opis |
|----|----|----|----|----|
| `query` | string | tak | — | Zapytanie wyszukiwania |
| `top_k` | int | nie | 10 | Maksymalna liczba wyników |
| `stream` | string | nie | `__user_default__` | Filtr przestrzeni nazw |
| `streams` | string\[\] | nie | null | Wyszukiwanie w wielu przestrzeniach nazw |
| `date_from` | string | nie | null | Dolna granica daty ISO |
| `date_to` | string | nie | null | Górna granica daty ISO |
| `entity` | string | nie | null | Filtr według nazwy encji |
| `trace` | bool | nie | false | Dołącz informacje śledzenia diagnostycznego |
| `dry_run` | bool | nie | false | Pomiń automatyczne zwiększenie ważności i śledzenie dostępu |
| `valid_at` | int (sekundy Unix) | nie | null | Dwuczasowe wyszukiwanie historyczne: zwróć tylko fragmenty, których przedział `[valid_from, valid_until]` obejmuje ten znacznik czasu. Otwarte granice (None) są nieograniczone po danej stronie. |
| `include_superseded` | bool | nie | false | Uwzględnij starsze wersje |
| `fact_type` | string | nie | null | Filtr: `preference`, `project`, `fact` |
| `subject` | string | nie | null | Filtr według encji będącej podmiotem |
| `min_confidence` | f64 | nie | null | Minimalna pewność ekstrakcji |

**Odpowiedź:**

``` json
{
  "results": [
    {
      "chunk_id": "abc-123",
      "content": "User switched from VSCode to Cursor (reason: speed)",
      "score_final": 0.87,
      "trace_info": {
        "level": "L1",
        "source": "consolidation",
        "is_latest": true,
        "created_at": 1743638400,
        "memory_type": "static",
        "importance": 1.2,
        "access_count": 3,
        "version": 2,
        "superseded_by": null
      }
    }
  ],
  "trace_metadata": {
    "total_results_before_topk": 42,
    "dedup_removed": 3,
    "search_latency_us": 1200,
    "query_complexity": "medium"
  }
}
```

**Obiekt evidence (eksperymentalny, opcjonalny):** gdy `[search] evidence_fields = true` w `config.toml`, każdy wynik (świeży lub z pamięci podręcznej) zawiera dodatkowy klucz `evidence`. Przy wyłączonej fladze klucz jest całkowicie pomijany, więc domyślne odpowiedzi się nie zmieniają. Obiekt ma dokładnie siedem kluczy, wszystkie są zawsze obecne; nieznana wartość to `null`, nigdy data zastępcza.

``` json
"evidence": {
  "author": "user",
  "conversation_date": "2026-04-02",
  "event_date": "2026-03-26",
  "event_date_provenance": "resolved",
  "source_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "byte_start": 0,
  "byte_end": 142
}
```

| Klucz | Typ | Opis |
|----|----|----|
| `author` | string? | Znormalizowana rola autora: `user`, `assistant`, `system` lub `tool`. Każda inna wartość (imię i nazwisko, dowolny tekst) to `null` |
| `conversation_date` | string? | Data rozmowy (`YYYY-MM-DD`), w zapisanej postaci |
| `event_date` | string? | Data opisanego zdarzenia (`YYYY-MM-DD`), w postaci uzyskanej z ekstrakcji. Zwracane są tylko poprawne kalendarzowo daty ISO |
| `event_date_provenance` | string? | `resolved` gdy silnik zapisał wskazówkę czasową, na podstawie której ustalono datę, `unknown` w przeciwnym razie; `null` dokładnie wtedy, gdy `event_date` wynosi `null`. `explicit` jest zarezerwowane i obecnie nie jest zwracane |
| `source_id` | string? | Kanoniczny fragment źródłowy, z którego wycięto lub wyekstrahowano tę pamięć |
| `byte_start`, `byte_end` | int? | Zakres bajtów fragmentu wewnątrz źródła (zobacz [`[fragments]`](configuration.md#fragments)); oba pola są ustawione albo oba mają wartość `null` |

**Odrzucenia na etapach (żądania ze śledzeniem):** przy `trace: true`, `trace_metadata` zawiera także `rejections` — jeden wpis dla każdego kandydata usuniętego między etapami po rankingu objętymi śledzeniem — obok istniejących migawek etapów `evidence`. `stage` oznacza pierwszy etap, na którym nie ma danego identyfikatora (`after_soft_delete`, `after_filters_and_dedup` lub `response`); `reason` przyjmuje jedną z wartości `soft_deleted`, `event_date_out_of_range`, `bitemporal_out_of_range`, `superseded`, `extraction_meta_missing`, `fact_type_filter`, `subject_filter`, `min_confidence`, `content_prefix_dedup`, `aggregation_subject_dedup`, `source_agent_filter`, `truncated_top_k`, `operation_finalization`. Migawka `response` jest zapisywana po finalizacji operacji i końcowej kontroli daty zdarzenia, więc jej identyfikatory i kolejność odpowiadają zwróconym wynikom wyszukiwania. Rekord przywrócony do końcowej odpowiedzi nie jest wymieniany jako odrzucony; `operation_finalization` oznacza wynik główny wyparty przez politykę operacji. Nazwy czterech etapów evidence i ich kolejność pozostają bez zmian. Przycinanie MCP według `max_context_tokens` następuje po etapach objętych śledzeniem, więc usunięty w ten sposób wynik nie jest tu wymieniany.

``` json
"trace_metadata": {
  "rejections": [
    { "chunk_id": "def-456", "stage": "after_filters_and_dedup", "reason": "superseded" },
    { "chunk_id": "ghi-789", "stage": "response", "reason": "truncated_top_k" }
  ]
}
```

------------------------------------------------------------------------

<span id="post-v1context-pack"></span>

### POST /v1/context-pack

Pakowanie kontekstu do umieszczenia w prompcie systemowym. Składa okno kontekstu z profilu, istotnych wspomnień i ostatniej aktywności w ramach limitu tokenów.

**Żądanie:**

``` json
{
  "query": "working on dashboard project",
  "stream": "100",
  "budget_tokens": 2000,
  "sections": ["profile", "relevant", "recent"]
}
```

| Pole | Typ | Domyślnie | Opis |
|----|----|----|----|
| `query` | string | null | Temat przewodni |
| `stream` | string | `__user_default__` | Przestrzeń nazw |
| `budget_tokens` | int | 4000 | Maksymalna liczba tokenów w odpowiedzi |
| `sections` | string\[\] | wszystkie | Sekcje do uwzględnienia |

**Podział budżetu tokenów między sekcje:**

- `profile`: 20% budżetu
- `relevant`: 50% budżetu
- `recent`: 30% budżetu

**Odpowiedź:**

``` json
{
  "context": "## Profile\nUser is a software engineer...\n\n## Relevant\n...\n\n## Recent\n...",
  "sources": [
    { "chunk_id": "abc-123", "score": 0.87, "section": "relevant" }
  ],
  "total_tokens": 1856,
  "sections_included": ["profile", "relevant", "recent"],
  "sections_truncated": []
}
```

------------------------------------------------------------------------

<span id="memory-management"></span>

## Zarządzanie pamięcią

<span id="post-v1delete"></span>

### POST /v1/delete

Usuń jeden fragment według identyfikatora.

``` json
{ "id": "abc-123" }
```

Odpowiedź: `{ "status": "deleted", "id": "abc-123" }`

<span id="delete-apimemoriesid"></span>

### DELETE /api/memories/{id}

Usuwanie zgodne z REST. Opcjonalny parametr zapytania `?ns=<namespace>`.

Odpowiedź: `{ "deleted": true, "id": "abc-123" }`

<span id="get-apimemoriesid"></span>

### GET /api/memories/{id}

Zwraca jeden element pamięci (`id`, `content`, `layer`, `confidence`, `decay`, `event_date`, `created_at`, `category`, `entity_ids`, `version`, `is_latest`, `superseded_by`, `source_stream`, `source`, `source_agent`). Dla nieznanych lub miękko usuniętych identyfikatorów zwraca 404; osoby bez uprawnień administratora mogą czytać tylko fragmenty we własnym strumieniu.

Gdy `[search] evidence_fields = true`, element zawiera również taki sam dodatkowy obiekt `evidence` jak wynik `/v1/search` (zobacz powyżej). Punkt końcowy listy w panelu go nie zawiera.

------------------------------------------------------------------------

<span id="post-v1purge-namespace"></span>

### POST /v1/purge-namespace

Usuń wszystkie fragmenty w strumieniu.

``` json
{
  "stream": "100",
  "dry_run": true,
  "confirmed": false
}
```

Odpowiedź:

``` json
{
  "status": "ok",
  "stream": "100",
  "dry_run": true,
  "deleted_count": 42,
  "deleted_ids": ["abc-123", "def-456", "..."]
}
```

Ustaw `dry_run: false` i `confirmed: true`, aby faktycznie usunąć dane.

------------------------------------------------------------------------

<span id="graph"></span>

## Graf

<span id="get-v1graphentitynamestreamstream_id"></span>

### GET /v1/graph/entity/{name}?stream={stream_id}

Pobierz węzeł encji wraz z krawędziami i odwołaniami do fragmentów. **Wymaga parametru `stream`** — graf jest izolowany w obrębie strumienia.

``` json
{
  "entity": {
    "id": "ent-123",
    "name": "Cursor",
    "type": "TECHNOLOGY",
    "aliases": ["cursor IDE"],
    "chunk_count": 2
  },
  "neighbors": [
    { "entity": "Alice", "entity_type": "PERSON", "relation": "uses", "direction": "incoming" }
  ],
  "chunk_ids": ["abc-123", "def-456"]
}
```

<span id="get-v1graphstats"></span>

### GET /v1/graph/stats

``` json
{
  "total_entities": 42,
  "total_edges": 87,
  "avg_chunks_per_entity": 3.2
}
```

<span id="post-v1build-graph"></span>

### POST /v1/build-graph

Odbuduj graf encji ze wszystkich zapisanych encji. Przydatne po wsadowym zapisie.

<span id="post-v1extract-entities"></span>

### POST /v1/extract-entities

Uruchom uzupełnianie ekstrakcji encji przez LLM dla fragmentów, w których jej brakuje. Operacja działa asynchronicznie.

------------------------------------------------------------------------

<span id="synthesis"></span>

## Synteza

Historia wersji fragmentu jest dostępna przez narzędzie `memory_history` MCP; zsyntetyzowany profil użytkownika przez narzędzie `memory_profile` MCP. Zobacz [Opis narzędzi MCP](mcp-tools.md).

<span id="get-v1generate-memory-md"></span>

### GET /v1/generate-memory-md

Wygeneruj propozycję MEMORY.md na podstawie najwyżej ocenionych fragmentów.

``` json
{
  "proposal": "# Memory\n\n## Identity\n- Software engineer...\n\n## Preferences\n...",
  "metadata": { "chunks_considered": 200, "sections": 15 }
}
```

------------------------------------------------------------------------

<span id="consolidation-maintenance"></span>

## Konsolidacja i utrzymanie

<span id="post-v1dream"></span>

### POST /v1/dream

Uruchom konsolidację dream w bieżącym strumieniu.

``` json
{
  "stream": "100",
  "chunks_processed": 50,
  "groups_found": 12,
  "facts_merged": 8,
  "contradictions_resolved": 2,
  "cost_usd": 0.04,
  "duration_ms": 3200
}
```

Analiza jakości pamięci jest dostępna przez narzędzie `memory_reflect` MCP.

<span id="post-v1boost"></span>

### POST /v1/boost

Zwiększ ważność fragmentu do 1,5.

``` json
{ "id": "abc-123" }
```

<span id="post-v1embed-missing"></span>

### POST /v1/embed-missing

Uzupełnij embeddingi fragmentów, które ich nie mają.

``` json
{
  "status": "ok",
  "total_missing": 15,
  "embedded": 15,
  "failed": 0
}
```

<span id="post-v1retag-all"></span>

### POST /v1/retag-all

Ponownie wyodrębnij encje we wszystkich fragmentach.

<span id="post-v1score-all"></span>

### POST /v1/score-all

Ponownie oblicz oceny ważności wszystkich fragmentów na podstawie podobieństwa embeddingów.

<span id="post-v1re-embed-all"></span>

### POST /v1/re-embed-all

Ponownie wygeneruj wszystkie embeddingi (przydatne przy zmianie dostawcy).

<span id="post-v1reset-importance"></span>

### POST /v1/reset-importance

Przywróć wartości domyślne wszystkim skumulowanym automatycznym zwiększeniom ważności.

<span id="post-v1reset-backfill"></span>

### POST /v1/reset-backfill

Wyczyść znaczniki ekstrakcji encji przez LLM, aby umożliwić ponowne przetworzenie.

------------------------------------------------------------------------

<span id="admin"></span>

## Administracja

<span id="get-v1status"></span>

### GET /v1/status

Stan silnika i statystyki.

``` json
{
  "status": "ok",
  "uptime_secs": 86400,
  "config_summary": {
    "vector_enabled": true,
    "tantivy_enabled": true,
    "scheduler_enabled": true,
    "rocksdb_keys": 311,
    "tantivy_docs": 115,
    "embeddings_count": 112
  }
}
```

<span id="get-v1namespaces"></span>

### GET /v1/namespaces

``` json
{
  "namespaces": {
    "personal": "100",
    "work": "110"
  }
}
```

<span id="get-v1whoami"></span>

### GET /v1/whoami

Zwraca kontekst uwierzytelnienia bieżącego klienta (aktywny strumień, rolę i dostępne strumienie).

<span id="get-health"></span>

### GET /health

Kontrola dostępności. **Uwierzytelnianie nie jest wymagane.**

``` json
{ "status": "ok" }
```

------------------------------------------------------------------------

<span id="ambient-memory"></span>

## Pamięć kontekstowa

`POST /v1/ambient` zwraca niewielki zestaw fragmentów pamięci zawierających fakty, ograniczony budżetem tokenów, do umieszczenia w kontekście agenta na początku tury. Zobacz [kontrakt punktu końcowego ambient](https://github.com/vvooki-sys/loomem/blob/main/loomem-server/docs/ambient-endpoint.md).

------------------------------------------------------------------------

<span id="background-workers"></span>

## Procesy działające w tle

<span id="post-adminworkerspause"></span>

### POST /admin/workers/pause

Wstrzymaj wszystkie procesy działające w tle (konsolidację, wygaszanie, kompaktowanie, kopie zapasowe, grupowanie, usuwanie i statystyki). Przydatne podczas ewaluacji wymagających niezmiennej bazy danych.

**Wymaga tokenu administratora.**

**Odpowiedź:**

``` json
{
  "paused": true,
  "message": "All workers paused"
}
```

<span id="post-adminworkersresume"></span>

### POST /admin/workers/resume

Wznów wszystkie procesy działające w tle.

**Odpowiedź:**

``` json
{
  "paused": false,
  "message": "All workers resumed"
}
```

<span id="get-adminworkersstatus"></span>

### GET /admin/workers/status

Sprawdź, czy procesy są wstrzymane.

**Odpowiedź:**

``` json
{
  "paused": false
}
```

Stan wstrzymania nie jest zachowywany po restarcie serwera — procesy uruchamiają się automatycznie przy starcie.

------------------------------------------------------------------------

<span id="mcp-endpoint"></span>

## Punkt końcowy MCP

<span id="post-mcp"></span>

### POST /mcp

Punkt końcowy MCP JSON-RPC 2.0. Korzystają z niego Claude Desktop, konektory claude.ai i inne klienty MCP. Serwer identyfikuje się jako `loomem-memory`.

Zobacz [Opis narzędzi MCP](mcp-tools.md) aby poznać dostępne narzędzia `memory_*`.

<span id="oauth-endpoints"></span>

### Punkty końcowe OAuth

Do autoryzacji zdalnego konektora MCP:

``` text
POST  /oauth/register      Dynamic Client Registration (RFC 7591)
GET   /oauth/authorize      Authorization page (user enters API key)
POST  /oauth/authorize      Submit authorization
POST  /oauth/token          Exchange code for access token
```
