<span id="loomem-benchmarks"></span>

# Benchmarki Loomem

**Loomem uzyskał 85,4% (427 / 500) w LongMemEval-S (cleaned)** w pełnym przebiegu diagnostycznym 29 października 2026 r. W teście użyto samodzielnie hostowanego silnika pamięci Loomem z lokalnymi embeddingami i modelu Kimi `k3-256k` zarówno do odpowiadania, jak i oceny odpowiedzi. Jest to wynik zmodyfikowanego, rygorystycznego ewaluatora, a nie oficjalny wynik rankingu ani bezpośrednie porównanie z innymi dostawcami.

Podsumowanie w formacie maszynowym jest dostępne w pliku [`benchmarks/longmemeval-2026-10-29-summary.json`](../benchmarks/longmemeval-2026-10-29-summary.json).

<span id="what-is-longmemeval"></span>

## Co mierzy LongMemEval

LongMemEval to publiczny benchmark pamięci długoterminowej asystentów konwersacyjnych. Na długich historiach obejmujących wiele sesji sprawdza ekstrakcję informacji, rozumowanie między sesjami i rozumowanie czasowe, aktualizacje wiedzy oraz powstrzymanie się od odpowiedzi. Oceniliśmy wszystkie 500 pytań z **LongMemEval-S (cleaned)**, korzystając z wariantu `xiaowu0162/longmemeval-cleaned`.

<span id="result-by-question-type"></span>

## Wyniki według rodzaju pytania

<div style="overflow-x:auto">

| Rodzaj pytania            | Poprawne      | Skuteczność |
|---------------------------|---------------|-------------|
| single-session-assistant  | 54 / 56       | 96.4%       |
| knowledge-update          | 74 / 78       | 94.9%       |
| single-session-user       | 66 / 70       | 94.3%       |
| temporal-reasoning        | 115 / 133     | 86.5%       |
| single-session-preference | 25 / 30       | 83.3%       |
| multi-session             | 93 / 133      | 69.9%       |
| **Wszystkie pytania**     | **427 / 500** | **85.4%**   |

</div>

Wszystkie 500 wywołań modelu odpowiadającego i wszystkie 500 wywołań modelu oceniającego zakończyły się po udokumentowanych technicznych ponowieniach. Wynik końcowy nie zawiera brakujących odpowiedzi ani błędów technicznych. Pierwotny przebieg pierwszych 100 pytań dał wynik 77/100 i obejmował dziewięć uciętych odpowiedzi modelu. Ponowienie tylko tych nieudanych wywołań z większym limitem odpowiedzi dało 80/100. Osobny błąd HTTP przerwał trzecią setkę; wznowiono tylko pytania nieudane lub jeszcze niepodjęte, uzyskując 77/100.

<span id="configuration"></span>

## Konfiguracja

<div style="overflow-x:auto">

| Ustawienie | Wartość |
|----|----|
| Silnik | Plik wykonywalny Loomem E1 fragments do ewaluacji |
| Embeddingi | Lokalny `multilingual-e5-small`, 384 wymiary |
| Model odpowiadający i oceniający | Kimi `k3-256k` |
| Limity odpowiedzi modelu odpowiadającego / oceniającego | 8000 / 512 tokenów |
| Wysiłek rozumowania / temperatura / seed | `low` / `1` / `42` |
| Zwrócone materiały źródłowe | 60 rekordów na pytanie |
| Materiały dla modelu odpowiadającego | Contract v2, budżet materiału 8000 tokenów |
| Pakowanie | Z zachowaniem kolejności rankingu, całe rekordy, first-fit |
| Protokół ewaluacji | `loomem-longmemeval-strict-v2-context-v2` |
| SHA-256 zbioru danych | `d6f21ea9d60a0d56f34a05b609c79c88a451d2ae03597821ea3d5a9678c3a442` |
| SHA-256 pliku wykonywalnego silnika | `2f16b74bc4a72a659314b494e7df6fb46bbfe3048ab73cf5baf17347e568eed4` |

</div>

Model odpowiadający otrzymuje bieżące pytanie oraz materiały zwrócone z pamięci, nigdy odpowiedzi referencyjne, kategorie pytań ani etykiety ewaluacyjne. Odpowiedzi referencyjne trafiają tylko do osobnego modelu oceniającego. Szablony oceny odpowiadają przypiętemu ewaluatorowi LongMemEval, ale interpretacja odpowiedzi jest bardziej rygorystyczna: akceptowane jest tylko dokładne `yes` lub `no`.

<span id="retrieval-diagnostics"></span>

## Diagnostyka wyszukiwania i kontekstu modelu odpowiadającego

Diagnostyka wyszukiwania jest mierzona offline na podstawie etykiet źródłowych i nie jest miarą poprawności odpowiedzi. Pochodzenie z danej sesji lub tury nie dowodzi też, że fragment zachowuje wymaganą treść semantyczną.

<div style="overflow-x:auto">

| Wskaźnik diagnostyczny | Wynik |
|----|----|
| Makro recall sesji zawierających odpowiedź | 99.62% |
| Pytania, dla których zwrócono wszystkie sesje zawierające odpowiedź | 496 / 500 |
| Makro recall oznaczonych tur przed pakowaniem | 92.07% |
| Pytania, dla których zwrócono wszystkie oznaczone tury | 421 / 479 |
| Makro recall oznaczonych tur po pakowaniu | 86.32% |
| Pytania, dla których wszystkie oznaczone tury wysłano do modelu odpowiadającego | 380 / 479 |
| Pytania, które utraciły co najmniej jedną wyszukaną oznaczoną turę podczas pakowania | 53 / 479 |

</div>

<span id="multi-session-errors"></span>

## Analiza błędów pytań wielosesyjnych

Pytania wielosesyjne są obecnie główną słabością: przypada na nie 40 z 73 niepoprawnych odpowiedzi. Ręczny przegląd tych odpowiedzi, połączony z analizą pochodzenia materiałów przed pakowaniem kontekstu i po nim, daje następujące wzajemnie wykluczające się kategorie diagnostyczne:

<div style="overflow-x:auto">

| Ustalenie | Przypadki |
|----|----|
| Co najmniej jedną wyszukaną oznaczoną turę pominięto podczas pakowania kontekstu modelu odpowiadającego | 22 |
| Wyszukiwanie zwróciło niepełny zestaw, bez dodatkowej utraty oznaczonych tur podczas pakowania | 11 |
| Wszystkie oznaczone tury dotarły do modelu odpowiadającego, lecz odpowiedź była niepoprawna | 4 |
| Przypadki powstrzymania się od odpowiedzi, w tym możliwe problemy z rygorystyczną oceną lub interpretacją materiału | 3 |

</div>

Te kategorie wskazują, co należy zbadać; nie dowodzą jednej przyczyny każdej błędnej odpowiedzi. Pierwszym priorytetem jest ulepszenie doboru materiałów dla modelu odpowiadającego w pytaniach wymagających połączenia faktów z różnych sesji. Kolejne priorytety to pokrycie wyszukiwania dla pozostałych pytań wielosesyjnych oraz rozumowanie czasowe lub arytmetyczne, gdy niezbędne materiały są już dostępne.

<span id="honesty"></span>

## Jak interpretować wynik

- **Samodzielna ewaluacja diagnostyczna.** To nie jest wynik z rankingu prowadzonego przez podmiot zewnętrzny. Wynik 85,4% dotyczy dokładnie opisanej wyżej konfiguracji i nie powinien być przedstawiany jako niezależna od modelu cecha Loomem.
- **Modele odpowiadający i oceniający mają znaczenie.** Obie role pełnił Kimi `k3-256k`. Raport nie został zamknięty niezależnym audytem semantycznym, a wynik nie jest bezpośrednio porównywalny z wcześniejszą konfiguracją GPT-4.1 ani z liczbami publikowanymi przez innych dostawców.
- **To nie był wcześniej niewidziany zbiór testowy.** LongMemEval był używany na wcześniejszych etapach rozwoju. Sam wynik nie dowodzi uogólnienia na nowe historie ani rzeczywiste rozmowy użytkowników.
- **Lokalne embeddingi, zewnętrzny model odpowiadający.** Silnik pamięci i embeddingi działały lokalnie; modele Kimi odpowiadający i oceniający korzystały z dostępnego API. Cała ewaluacja nie była więc przeprowadzona wyłącznie offline.
- **Wcześniejszy wynik.** Konfiguracja z 26 czerwca 2026 r. uzyskała 75,0% (375 / 500), z GPT-4.1 jako modelem odpowiadającym i oceniającym. Liczbowy wzrost jest zachęcający, lecz różnice w protokole i modelu nie pozwalają traktować go jako kontrolowanego badania ablacyjnego.

Loomem jest projektem open source na licencji Apache-2.0. LongMemEval: Wu i in., „LongMemEval: Benchmarking Chat Assistants on Long-Term Interactive Memory”. Zbiór danych: [`xiaowu0162/longmemeval-cleaned`](https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned).
