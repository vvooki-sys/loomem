<span id="mcp-tools-reference"></span>

# Narzędzia MCP — dokumentacja

Loomem udostępnia narzędzia `memory_*` za pośrednictwem protokołu Model Context Protocol. Claude i inne klienty MCP używają ich do pracy z pamięcią. Serwer MCP przedstawia się jako `loomem-memory`.

> **Synchronizacja z klientami:** Skrócona wersja wysyłana do klientów MCP znajduje się w `loomem-server/mcp_instructions.md` (jest wbudowywana podczas kompilacji). Aby zmienić treść widoczną dla klientów, edytuj ten plik.

------------------------------------------------------------------------

<span id="scope-model"></span>

## Model zakresów

Dane znajdują się w **strumieniach**. Różne narzędzia działają w różnych zakresach:

| Zakres | Narzędzia | Znaczenie |
|----|----|----|
| **W obrębie strumienia** (izolowany) | `memory_store`, `memory_search`, `memory_ingest`, `memory_context`, `memory_profile`, `memory_dream`, `memory_reflect`, `memory_associate`, `memory_stats` | Odczytuje i zapisuje dane wyłącznie w aktywnym strumieniu |
| **Globalny** (cały silnik) | `memory_status`, `memory_namespaces` | Zwraca informacje o systemie |
| **W obrębie strumienia** (izolowany graf) | `memory_graph` | Graf wiedzy jest izolowany dla każdego strumienia |
| **Według ID fragmentu** | `memory_history`, `memory_delete`, `memory_feedback` | Działa na konkretnym fragmencie |

**Najważniejsze pojęcia:**

- **Strumień** — granica izolacji danych. Wbudowany strumień domyślny to `__user_default__`.
- **Przestrzeń nazw** — czytelna nazwa przypisana do identyfikatora strumienia (np. `personal` → `100`). Konfiguruje się ją w `config.toml`, a wyświetla za pomocą `memory_namespaces`.

------------------------------------------------------------------------

<span id="storing-memories"></span>

## Zapisywanie wspomnień

<span id="memory_store"></span>

### memory_store

Zapisz pojedynczy, potwierdzony fakt.

**Używaj do:** jawnych deklaracji użytkownika, potwierdzonych decyzji i faktów biograficznych. **Nie używaj do:** surowego tekstu rozmowy — zamiast tego użyj `memory_ingest` .

| Parametr | Typ | Wymagany | Opis |
|----|----|----|----|
| `content` | string | tak | Fakt do zapisania |
| `source` | string | nie | Pochodzenie (np. „user-stated”) |
| `subject` | string | nie | Nazwa encji (osoba, projekt) |
| `metadata` | object | nie | Niestandardowe metadane JSON |
| `supersedes` | string | nie | Identyfikator fragmentu zastępowanego przez ten fakt. Łączy wersje: stary fragment staje się wersją zastąpioną (ukrytą w `memory_search` chyba że użyto `include_superseded`), a nowy jest aktualny. Wskazany fragment musi być aktualną wersją swojego łańcucha; w przeciwnym razie wywołanie zwraca błąd i wskazuje nowszą wersję. |

**Przykład:**

``` text
memory_store(
  content: "Prefers Cursor over VSCode for AI features",
  subject: "Alice",
  source: "user-stated"
)
```

**Działanie:**

- Preferencje automatycznie otrzymują `importance: 2.0`
- Uruchamiana jest ekstrakcja encji (słownik i kolejka LLM)
- Wykrywanie sprzeczności sprawdza istniejące wspomnienia
- Przy użyciu `supersedes`, potwierdzenie ma postać `Stored: "..." (id: <uuid>, supersedes: <old uuid>)` a `memory_history` pokazuje obie wersje
- Embedding jest generowany asynchronicznie

------------------------------------------------------------------------

<span id="memory_ingest"></span>

### memory_ingest

Wyodrębnij z rozmowy uporządkowaną wiedzę przy użyciu LLM. **Preferowana metoda** zapisywania treści zawierających wiele faktów.

| Parametr | Typ | Wymagany | Opis |
|----|----|----|----|
| `content` | string | tak | Transkrypcja rozmowy |
| `conversation_date` | string | nie | Znana data rozmowy w formacie `YYYY-MM-DD` ; jest to czas obserwacji, a nie zamiennik nieznanej daty zdarzenia. |

**Przykład:**

``` text
memory_ingest(
  content: "User: I switched from VSCode to Cursor last week. Also, the dashboard deadline is April 15th.",
  conversation_date: "2026-04-02"
)
```

**Co się dzieje:**

1.  Najpierw oczyszczona transkrypcja jest zapisywana jako trwałe źródło pomocnicze, które można przeszukiwać. Treść jest oczyszczana do osiągnięcia punktu stałego przed zapisem: dane, które nadal zmieniają się po czterech przebiegach (np. głęboko zagnieżdżone kodowanie encji HTML), są odrzucane jako nieprawidłowe parametry i nic nie zostaje zapisane. Dzięki temu ekstrakcja zawsze widzi dokładnie zapisany tekst.
2.  **Opcjonalnie** (`[fragments] enabled = true`, domyślnie wyłączone — zobacz [Konfiguracja](configuration.md#fragments)): bezpośrednio po źródle, a przed ekstrakcją, zapisywany jest jeden fragment na każdą wypowiedź w rozmowie. Każdy zawiera rolę autora, datę rozmowy i dokładny zakres bajtów w źródle. Tekst odpowiedzi kończy się wtedy komunikatem ` Stored N fragments.` — albo ` Stored N fragments, M fragment writes failed.`, ` Stored N fragments, M fragments skipped (sanitizer would rewrite the slice).`, albo ` Stored 0 fragments (segmentation failed: …).` / ` Stored 0 fragments (segmentation skipped: …).` gdy wystąpił problem. Zapis fragmentów nigdy nie blokuje ekstrakcji faktów; przy wyłączonych fragmentach odpowiedź pozostaje bez zmian.
3.  Gdy ekstrakcja jest włączona, LLM wyodrębnia fakty określonych typów powiązane ze źródłem, wraz z podmiotem, poziomem pewności, opcjonalnym przypisaniem wypowiedzi i ustalonymi datami zdarzeń.
4.  Odpowiedź zawiera identyfikator źródła także wtedy, gdy ekstrakcja się nie powiedzie, brakuje danych uwierzytelniających lub nie znaleziono faktów. Częściowe zapisy faktów są zgłaszane; źródło zostaje zachowane.

Ta ścieżka przetwarzania nie usuwa automatycznie duplikatów ani nie zastępuje istniejących faktów. Ponowne przetworzenie tworzy kolejne źródło. Treść i pochodzenie powiązane ze źródłem są niezmienne: aby poprawić rozmowę, usuń jej identyfikator źródła i przetwórz poprawioną transkrypcję.

Data rozmowy stanowi punkt odniesienia dla wyrażeń względnych tylko wtedy, gdy jest znana; nieznana data zdarzenia pozostaje nieznana. Brak daty rozmowy nie upoważnia do używania czasu zapisu do ustalania dat względnych. Nowe rekordy przedstawiają znane zdarzenia jako `[event:YYYY-MM-DD]`, obserwacje jako `[observed:YYYY-MM-DD]`, a czas zapisu jako `[recorded:YYYY-MM-DD]`. Dawne oznaczenia `[YYYY-MM-DD]` pozostają bez zmian i nie ustalają daty zdarzenia. W rekordach z rozpoznanym wersjonowanym kontraktem przetwarzania wyszukiwanie w przedziale dat wymaga znanej daty zdarzenia; nieznana data nie pasuje do przedziału. Daty rozmowy i zapisu nie zastępują czasu zdarzenia. Dawne rekordy bez tego kontraktu zachowują istniejące daty zdarzeń oraz zapasowe użycie czasu zapisu, gdy nie mają daty. Przy uruchomieniu starsze wersjonowane wpisy indeksu są naprawiane w miejscu bez resetowania indeksu, a w razie błędu naprawy uruchomienie zostaje przerwane. Oddzielny filtr valid_at zachowuje swoją semantykę ważności i historii. Jeśli wyszukiwanie filtrowane datą pomija źródło o nieznanej dacie zdarzenia, ponów je bez filtra czasu albo pobierz zwrócony identyfikator źródła.

**Przykładowy wynik (ilustracyjny — dokładna ekstrakcja zależy od oceny LLM):**

- `PreferenceOrDecision`: „Użytkownik woli Cursor od VSCode” (podmiot: użytkownik, pewność: 0.95)
- `ProjectState`: „Termin dashboardu to 2026-04-15” (podmiot: dashboard, pewność: 0.90)

Liczba wyodrębnionych faktów, ich sformułowanie i poziomy pewności zależą od danych wejściowych. Traktuj przykłady jako orientacyjne, a nie gwarantowany wynik.

------------------------------------------------------------------------

<span id="searching-memories"></span>

## Wyszukiwanie wspomnień

<span id="memory_search"></span>

### memory_search

Wyszukiwanie hybrydowe we wszystkich poziomach pamięci.

| Parametr      | Typ    | Wymagany | Opis                                      |
|---------------|--------|----------|-------------------------------------------|
| `query`       | string | tak      | Zapytanie wyszukiwania                    |
| `top_k`       | int    | nie      | Maksymalna liczba wyników (domyślnie: 10) |
| `time_filter` | string | nie      | "today", "this_week", "this_month"        |

**Przykład:**

``` text
memory_search(query: "What IDE does the user prefer?")
```

**Działanie:**

- Równolegle przeszukuje BM25, wektory i graf
- Uwzględnia spadek w czasie (nowsze wspomnienia uzyskują wyższy wynik)
- Usuwa niemal identyczne wyniki
- Oznacza zastąpione fakty jako `[UPDATED]`
- Dla zapytań agregujących („ile...”) zwiększa `top_k` do 30
- Każdy wiersz wyniku kończy się wartością `chunk_id` — można jej bezpośrednio użyć w `memory_history`, `memory_delete`, lub `memory_feedback`
- **Opcjonalnie** (`[search] evidence_fields = true`, domyślnie wyłączone — zobacz [Konfiguracja](configuration.md#search)): każdy wiersz zawiera przed wynikiem ujęty w nawiasy znacznik dowodowy — `[author: user; conversation: YYYY-MM-DD; event: YYYY-MM-DD (resolved|unknown); source: <id>]` — wymieniający tylko elementy znane dla danego wspomnienia (wiersz bez żadnego z nich pozostaje bez zmian). Zakres bajtów źródła (`byte_start`/`byte_end`) jest udostępniany tylko przez interfejs REST.

------------------------------------------------------------------------

<span id="memory_context"></span>

### memory_context

Wczytaj istotny kontekst na początku zadania. Obowiązuje budżet tokenów, więc nie zapełni on okna kontekstowego.

| Parametr | Typ | Wymagany | Opis |
|----|----|----|----|
| `query` | string | nie | Temat (np. „projekt dashboardu”) |
| `budget_tokens` | int | nie | Maksymalna liczba tokenów (domyślnie: 4000) |
| `sections` | string\[\] | nie | Które sekcje: `profile`, `relevant`, `recent` |

**Przykład:**

``` text
memory_context(query: "Loomem project", budget_tokens: 2000)
```

**Podział sekcji:**

- `profile` (20%) — kim jest użytkownik, trwałe fakty
- `relevant` (50%) — wspomnienia pasujące do zapytania
- `recent` (30%) — aktywność z ostatnich 7 dni

**Kiedy wywołać:** Raz na początku rozmowy, gdy temat wymaga wiedzy kontekstowej.

------------------------------------------------------------------------

<span id="memory_profile"></span>

### memory_profile

Pobierz syntetyczny profil użytkownika.

| Parametr | Typ    | Wymagany | Opis                      |
|----------|--------|----------|---------------------------|
| `format` | string | nie      | `"markdown"` lub `"json"` |

**Zwraca:**

- `stable_facts` — trwałe fakty (maks. 30)
- `recent_context` — ostatnie 7 dni (maks. 15)
- `summary` — podsumowanie w 2–3 zdaniach

Profil jest generowany przez LLM i buforowany przez godzinę.

------------------------------------------------------------------------

<span id="memory_associate"></span>

### memory_associate

Odkryj nieoczywiste skojarzenia — wspomnienia zaskakująco istotne, choć na pierwszy rzut oka niepowiązane — przez przechodzenie grafu, współwystępowanie w czasie i bliskość semantyczną.

**Używaj do:** nieoczekiwanych powiązań i kreatywnych połączeń między tematami (np. „jakie wcześniejsze projekty wiążą się z tym nowym pomysłem?”). Użyj `memory_search` do bezpośredniego wyszukiwania faktów.

**Zwraca:** numerowaną listę skojarzeń z mechanizmem i wynikiem. Jeśli lista jest pusta, grupowanie mogło się jeszcze nie odbyć — najpierw wywołaj `memory_dream` .

------------------------------------------------------------------------

<span id="memory-maintenance"></span>

## Utrzymanie pamięci

<span id="memory_dream"></span>

### memory_dream

Uruchom konsolidację pamięci. Podobnie jak sen dla mózgu, łączy, usuwa duplikaty i porządkuje wspomnienia.

**Brak parametrów.**

**Co się dzieje:** 1. Grupuje wspomnienia według podmiotu 2. Łączy powiązane obserwacje w zwięzłe podsumowania 3. Rozstrzyga sprzeczności (wygrywają nowsze fakty) 4. Tworzy skompresowane fragmenty L1 z surowych zdarzeń L0 5. Przestrzega limitu kosztów (domyślnie 0,10 USD na uruchomienie)

**Zwraca:**

``` text
{
  "chunks_processed": 50,
  "groups_found": 12,
  "facts_merged": 8,
  "contradictions_resolved": 2,
  "cost_usd": 0.04,
  "duration_ms": 3200
}
```

**Kiedy wywołać:**

- Po długiej, produktywnej sesji
- Gdy użytkownik prosi o „uporządkowanie” lub „konsolidację” pamięci
- Automatycznie po 30 minutach bezczynności

------------------------------------------------------------------------

<span id="memory_reflect"></span>

### memory_reflect

Przeanalizuj jakość pamięci i uzyskaj sugestie ulepszeń.

| Parametr     | Typ | Wymagany | Opis                         |
|--------------|-----|----------|------------------------------|
| `max_chunks` | int | nie      | Liczba fragmentów do analizy |

**Zwraca:**

``` text
{
  "quality_score": 78,
  "breakdown": {
    "by_level": { "L0": 45, "L1": 62 },
    "with_extraction_meta": "87%",
    "with_subject": "72%"
  },
  "suggestions": [
    "28 chunks lack subject metadata — run memory_dream",
    "5 potential contradictions detected"
  ]
}
```

**Wzór oceny jakości:** `has_meta*0.4 + has_subject*0.3 + structured*0.3`

**Kiedy wywołać:** Okresowo (raz w tygodniu) lub gdy jakość pamięci wydaje się niska.

------------------------------------------------------------------------

<span id="memory_graph"></span>

### memory_graph

Przeglądaj powiązania encji w grafie wiedzy. **Izolacja według strumienia** — każdy użytkownik widzi tylko własne encje i powiązania.

| Parametr | Typ    | Wymagany | Opis                    |
|----------|--------|----------|-------------------------|
| `entity` | string | tak      | Nazwa encji do zbadania |

**Przykład:**

``` text
memory_graph(entity: "Cursor")
```

**Zwraca:** szczegóły encji (nazwa, typ, aliasy), połączenia z innymi encjami i liczbę powiązanych fragmentów. Widoczne są tylko encje i krawędzie w aktywnym strumieniu.

Nowe nazwy grafu, aliasy i relacje są powiązane z tworzącym je wspomnieniem, strumieniem, encją lub krawędzią oraz bieżącą generacją treści przy użyciu skonfigurowanego szyfrowania danych w spoczynku. Wyszukiwanie i wyświetlanie każdego wkładu wymagają aktywnego potwierdzającego wspomnienia w bieżącym strumieniu; alias usuniętej rozmowy nie może korzystać z potwierdzenia innego wspomnienia tej samej encji. Starsze dane grafu wnoszą jedynie jawnie zapisane nazwy encji dla wspomnień i krotki relacji. Niepotwierdzone aliasy i połączenia pozostają ukryte do ponownej ekstrakcji, a dawne typy encji mogą pojawić się jako Unknown; podobieństwo tekstu źródłowego nie służy do odtwarzania brakującego pochodzenia.

Przydatne do sprawdzenia, „kto nad czym pracuje”, „jakie technologie są powiązane” itp.

------------------------------------------------------------------------

<span id="memory_history"></span>

### memory_history

Prześledź, jak fakt zmieniał się w czasie (łańcuch wersji).

| Parametr   | Typ    | Wymagany | Opis                                |
|------------|--------|----------|-------------------------------------|
| `chunk_id` | string | tak      | Fragment do prześledzenia           |
| `limit`    | int    | nie      | Maksymalna liczba zwracanych wersji |

**Przykład:**

``` text
memory_history(chunk_id: "abc-123")
```

**Zwraca:** łańcuch wersji pokazujący zmiany faktu:

``` text
v1 (2026-03-15): "User uses VSCode"        ← superseded
v2 (2026-04-01): "User uses Cursor"         ← current
```

**Kiedy wywołać:** gdy użytkownik pyta „czego używałem przed X?” albo podczas badania sprzecznych informacji.

------------------------------------------------------------------------

<span id="memory_delete"></span>

### memory_delete

Oznacz konkretne wspomnienie jako usunięte według ID (soft delete). Usunięcie rozpoznanego źródła przetwarzania oznacza również powiązane z nim rekordy potomne w tym samym strumieniu jako usunięte przed oczyszczeniem indeksu. Usunięcie pojedynczego faktu pochodnego zachowuje jego źródło i inne fakty; odpowiedź podaje ID zachowanego źródła. Usuń to ID, aby usunąć rozmowę i powiązane wspomnienia z wyników wyszukiwania.

Odpowiedź wskazująca częściowe oczyszczenie wymaga ponowienia. Treści oznaczone jako usunięte podlegają istniejącym zasadom odzyskiwania i usuwania; nie oznacza to natychmiastowego fizycznego wymazania ani wyszukania każdej semantycznej kopii. Niepowiązane kopie mogą pozostać. Nazwy, aliasy i połączenia grafu pozostają widoczne tylko wtedy, gdy mają własne aktywne wspomnienia potwierdzające.

| Parametr   | Typ    | Wymagany | Opis                     |
|------------|--------|----------|--------------------------|
| `chunk_id` | string | tak      | Wspomnienie do usunięcia |

**Ważne:** Nigdy nie usuwaj bez potwierdzenia użytkownika.

------------------------------------------------------------------------

<span id="memory_feedback"></span>

### memory_feedback

Oceń przydatność fragmentu pamięci po zadaniu, przekazując sygnał wzmacniający do systemu rankingowego.

| Parametr | Typ | Wymagany | Opis |
|----|----|----|----|
| `chunk_id` | string | tak | Fragment do oceny (z wiersza wyniku `memory_search` lub ID zwrócone przez `memory_store` / `memory_ingest`) |
| `usefulness` | int | tak | 0 (nieprzydatny) – 4 (kluczowy) |
| `harmful` | bool | nie | Ustaw tylko wtedy, gdy fragment zawierał nieprawidłowe lub mylące informacje. |

Oceniaj tylko fragmenty o wyraźnej przydatności; nie jest to narzędzie do ogólnego zapisu.

------------------------------------------------------------------------

<span id="informational"></span>

## Informacje

<span id="memory_status"></span>

### memory_status

Stan i statystyki silnika. Użyj, aby sprawdzić, czy silnik działa, ocenić pojemność lub zdiagnozować problemy, gdy inne narzędzia nie odpowiadają.

**Brak parametrów.**

**Kiedy wywołać:**

- Narzędzia zwracają nieoczekiwane błędy lub puste wyniki
- Użytkownik pyta o stan albo pojemność pamięci
- Przed złożoną operacją (zbiorowe przetwarzanie, dream), aby sprawdzić gotowość

**Zwraca** tekstowy blok stanu dla Twojego strumienia, np.:

``` text
Loomem Status: ok
Your stream: __user_default__
Your memories: 142
Embeddings (this stream): warming up (118/142 indexed, 24 pending, 83%)
Associator: active (7 clusters)
Event log drops: 0
Audit write failures: 0
Undecodable chunks (last full scan): 0
LLM failures (last 60m): extraction=0, ner=0, embedding=0, consolidation=0
```

Najważniejsze wskaźniki:

- `Embeddings (this stream)` pokazuje `ready` gdy wszystkie fragmenty zostały zindeksowane; `warming up (... pending ...)` oznacza, że indeks wektorowy jest jeszcze uzupełniany, a wyszukiwanie korzysta z BM25, dopóki liczba oczekujących nie spadnie do 0.
- `Associator: enabled, awaiting clustering` oznacza, że `memory_associate` nie ma nic do zwrócenia, dopóki nie zostanie uruchomione `memory_dream` .
- Niezerowe wartości `Event log drops`, `Audit write failures`, lub `Undecodable chunks` sygnalizują problemy z integralnością danych, które warto zbadać.

(Obiekt JSON z `uptime_secs` / `rocksdb_keys` / `vector` / `scheduler` jest zwracany przez administracyjny punkt statystyk HTTP — zobacz dokumentację API — a nie przez to narzędzie MCP.)

------------------------------------------------------------------------

<span id="memory_stats"></span>

### memory_stats

Pełniejszy wykaz i rozkład użycia Twojego strumienia — uzupełnienie `memory_status` (które pokazuje tylko stan). Użyj, gdy potrzebujesz liczników i rozkładów, a nie samej kontroli stanu.

**Brak parametrów.**

**Kiedy wywołać:**

- Gdy potrzebujesz zestawienia zawartości aktywnego strumienia (sumy, typy, rozkład wieku)
- Do audytu lub raportowania zawartości strumienia bez odczytywania pojedynczych wspomnień

**Prywatność:** zwraca wyłącznie zagregowane liczniki, znaczniki czasu i rozkłady — nigdy treść fragmentów.

------------------------------------------------------------------------

<span id="memory_namespaces"></span>

### memory_namespaces

Wyświetl dostępne przestrzenie pamięci.

**Brak parametrów.**

**Zwraca:**

``` text
{
  "namespaces": {
    "personal": "100",
    "work": "110"
  }
}
```

------------------------------------------------------------------------

<span id="best-practices"></span>

## Dobre praktyki

<span id="do"></span>

### Zalecenia

- Używaj `memory_ingest` do rozmów (nie `memory_store`)
- Wywołuj `memory_context` na początku rozmowy przy odpowiednich zadaniach
- Uruchamiaj `memory_dream` po produktywnych sesjach
- Sprawdzaj `memory_reflect` okresowo
- Używaj `memory_search` przed udzieleniem odpowiedzi na podstawie danych treningowych

<span id="dont"></span>

### Czego unikać

- Nie zapisuj surowego tekstu rozmowy przez `memory_store`
- Nie wywołuj `memory_search` przy każdej odpowiedzi — tylko gdy przywołanie informacji ma znaczenie
- Nie zapisuj tymczasowych statusów, pogawędek ani pytań bez odpowiedzi
- Nie zapisuj haseł, kluczy API ani innych sekretów
- Nie usuwaj bez potwierdzenia użytkownika
