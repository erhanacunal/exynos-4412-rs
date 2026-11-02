# Exynos 4412 için bare-metal Rust

Samsung Exynos 4412 (4 × Cortex-A9) üzerinde, kartın kendi U-Boot'undan `bootm` ile
başlatılan bare-metal Rust. `exynos_4412_rs` çalışma ortamını ve sürücüleri sağlar.
Uygulama bu crate'e bağımlı olur, bir `memory.x` verir ve `main` yazar.

- `exynos_4412_rs/`: startup, exception'lar, UART, konsol, panic, `link.x`
- `hello_world/`: konsol testi
- `irq_timer/`: GIC ve MCT testi
- `mmu_test/`: MMU ve önbellek testi
- `l2_test/`: L2 önbellek testi ve MCT hız ölçümü
- `smp_test/`: CPU1-3 açılışı, çekirdekler arası bellek ve SGI testi, L2 gecikme ölçümü
- `timer_test/`: Çekirdek başına MCT yerel zamanlayıcı testi
- `gpio_test/`: GPIO pinleri ve tuş kesmeleri testi

## Derleme

```sh
export PATH=$HOME/.cargo/bin:$PATH
cargo build --release   # ELF
make bin                # hello_world.bin ve boyutu
make img                # boot.img (~/mkbootimg/mkbootimg ile)
make disasm             # giriş kodu ve vektör tablosu
make img APP=irq_timer  # başka bir uygulama için aynı hedefler
```

Çıktılar `target/armv7a-none-eabihf/release/` altındadır. Hedef `.cargo/config.toml`
dosyasında, araç zinciri `rust-toolchain.toml` dosyasındadır.

Farklı bir yükleme adresi için `hello_world/memory.x` içindeki `ORIGIN` değiştirilir;
U-Boot'un `bootcmd`'i `0x40008000` kullandığı için kart tarafı da buna uymalıdır.

## Karta yükleme

`bootcmd` değişmez, akış Linux ile aynıdır:

```
movi init; mmc read 0 40008000 10040 4FC0; bootm 40008000 41000000
```

1. `make img`
2. Kart fastboot modundayken, Linux'ta kullandığın bölüm adıyla:
   ```sh
   fastboot flash <bölüm> target/armv7a-none-eabihf/release/boot.img
   ```
3. U-Boot'ta `boot`.

Konsol: UART2, 115200n8. `bootm` yolunda `dcache off` gerekmez, `bootm` önbellekleri
kernel için zaten kapatır. Bu işlem bölümdeki Linux imajının yerine yazar. Geri dönmek
için aynı komutla `~/boot.img` yazılır; U-Boot'a dokunulmaz.

`go` ile başlatmak da desteklenir (`dcache off`, yükleme, `go 0x40008000`), ancak bu
yol test edilmedi.

## İmaj biçimi

Bu U-Boot imzalıdır ve kaynağı yoktur, `bootm`'un imajı nasıl tanıdığı bilinmiyor. Bilinen
tek çalışan örnek Linux `zImage`'dir, bu yüzden imaj onun başlığıyla başlar:

| Offset | İçerik |
|---|---|
| `0x00-0x1F` | 8 adet `mov r0, r0` |
| `0x20` | `b __exynos_boot` |
| `0x24` | magic `0x016f2818` |
| `0x28` | başlangıç: `0` |
| `0x2C` | imaj boyutu |
| `0x30` | `0x04030201` |

Kartta doğrulandı: `boot.img` bölüme Android başlığıyla yazılıyor ve `bootm` imajı bir
sayfa sonrasından, `0x40009000`'den başlatıyor. `zImage` konumdan bağımsız olduğu için Linux
bundan etkilenmez, ama bizim kodumuz `0x40008000`'e link edilir. Bu yüzden startup'taki
kendini taşıma adımı **gereklidir**, silinmemelidir.

## Kart test sonucu (`hello_world`)

U-Boot (`Willow #`) ile `boot` komutundan alınan sonuçlar. `Unknown command 'movi'` uyarısı
`bootcmd`'den gelir, açılışı etkilemez.

| Eylem | Gözlenen |
|---|---|
| `bootm` | `Boot with Android zImage`, `Boot with zImage`, `Boot with Android Ramdisk Image`, `Starting kernel ...` |
| Açılış | `Merhaba Exynos 4412` |
| Başlatma adresi | `0x40009000` |
| `r0`-`r2` | `0x43e00898`, `0x000010b4`, `0x40000100` |
| `MPIDR` | `0x80000a00` (CPU0, DTS'teki `cpu@a00` ile uyumlu) |
| `CPSR` | `0x600001d3`, Supervisor |
| Karakter yazma | Echo çalışıyor |
| `p` | `*** Panic at hello_world/src/main.rs:26` ve mesaj, sonra durur |
| `a` | `DataAbort`, `DFSR=0x00000001` (hizalama), `DFAR=0x4000e175`, register dökümü, sonra durur |

Kartta henüz sınanmayanlar: FIQ, varsayılan undefined ve SVC raporları,
`_start`'a yanlışlıkla gelen ikincil çekirdeğin bekleme döngüsü.

## Bağımlılık

MMIO için `tock-registers` seçildi: bağımlılığı yoktur, register haritasını ve bit
alanlarını tipli olarak tanımlar, ayrıca byte genişlikli `UTXH`/`URXH` erişimini ifade edebilir.

## DTS'ten çıkarılan bilgiler

Kaynak: `exynos4412-mehmet.dts`. Linux ağacındaki kopyasıyla aynı dosyadır.
5.15'te `exynos4x12.dtsi` ve `exynos4x12-pinctrl.dtsi` yoktur; bunların yerine
`exynos4412.dtsi` (`exynos4.dtsi`'yi içerir) ve `exynos4412-pinctrl.dtsi` kullanıldı.

| Bilgi | Değer | Kaynak |
|---|---|---|
| Konsol | UART2, `0x1382_0000`, 115200n8, pinler `gpa1-0`/`gpa1-1` | `chosen/stdout-path = "serial2:115200n8"`, `aliases/serial2`, `serial_2`, `uart2_data` |
| DRAM | `0x4000_0000`, boyut `0x7EA0_0000` (2026 MiB), son adres `0xBEA0_0000` hariç | `memory@40000000` |
| Güvenli firmware | `0x0204_F000` | `firmware@204f000` |

`status = "okay"` olan çevre birimleri:

- **UART:** `serial_0` (Bluetooth), `serial_2` (konsol)
- **I2C:** `i2c_0`, `i2c_1`, `i2c_3`, `i2c_5`, `i2c_6`, `i2c_7`, `i2c_8`
- **Depolama:** `sdhci_2`, `sdhci_3` (WiFi), `mshc_0` (eMMC)
- **Görüntü:** `fimd`, `gpu`, `mfc`, `pwm` (arka ışık)
- **Kamera:** `camera`, `csis_1`, `fimc_0`, `fimc_1`, `fimc_3`
- **Ses:** `i2s0`
- **USB:** `ehci`, `exynos_usbphy`
- **Sistem:** `adc`, `rtc`, `tmu`, `watchdog`
- **Veri yolu frekansı:** `bus_acp`, `bus_c2c`, `bus_display`, `bus_dmc`, `bus_fsys`,
  `bus_leftbus`, `bus_mfc`, `bus_peri`, `bus_rightbus`

## Varsayımlar

- **Önbellek:** Startup MMU ve önbellekleri kapatır ama kirli satırları temizlemez; bunu
  `bootm` (veya `go` öncesi `dcache off`) yapmış olmalıdır. L2 bu noktada kapalıdır.
- **Saat ve pinler:** Konsol UART'ının saatini, pinlerini ve baud hızını U-Boot ayarlar.
- **Bellek:** `bootcmd` bölümden yaklaşık 10 MiB okuyup `0x40008000`'den yazar; imajın
  arkasına çöp gelmesi sorun değildir, `.bss` sıfırlanır. `make bin` sınırı aşan imajı reddeder.
- **Tek çekirdek:** Yalnızca CPU0 çalışır, diğerleri `wfe` döngüsünde bekler. Konsolda
  kilit yoktur.
- **Güvenlik durumu:** Kart Non-secure çalışır (`mmu_test` ile ölçüldü). VFP erişimine
  `NSACR` izin veriyor. Başka bir kartta izin yoksa startup, Rust'a girmeden `wfi` döngüsünde
  durur ve konsola bir şey basmaz.
- **Hizalama:** MMU kapalıyken bellek Strongly-ordered'dır, hizasız erişimin sonucu
  tanımsızdır. Bu yüzden startup `SCTLR.A`'yı açar ve hizasız erişim kesin bir data abort
  üretir. Hedefin `strict-align` kod üretimi derleyicinin böyle erişim üretmesini önler.
- **Atomikler:** `LDREX`/`STREX` Strongly-ordered bellekte güvenilir olmadığı için
  kütüphane yalnızca 32 bit ve daha küçük atomiklerde `load`/`store` kullanır. Uygulama da MMU
  açılana kadar `compare_exchange`, `fetch_add`, `swap` ve `AtomicU64` kullanmamalıdır.
- **Asenkron abort:** `CPSR.A` U-Boot'tan geldiği gibi bırakılır. Maskeliyse asenkron dış
  abort'lar görünmez.

## Tasarım notları

- **FIQ yığını:** İstenen SVC/IRQ/ABT/UND yığınlarına FIQ da eklendi (`__fiq_stack_size`),
  çünkü FIQ handler'ının da yığına ihtiyacı var.
- **VFP kaydı:** Exception stub'ları `d0-d7`, `d16-d31` ve `FPSCR`'ı da kaydeder, çünkü
  hedefin kod üretimi `d16-d31`'i kullanır.
- **SVC:** Uygulama SVC modunda çalıştığı için kendi `svc` çağrısı `LR`'sini ezer. Satır içi
  `svc` kullanan kod `lr`'yi clobber olarak bildirmelidir.
- **`BootArgs.load_addr`:** İstenen `r0`-`r2`'ye ek olarak, imajın başlatıldığı adres.

## Handler override

Her handler bir linker sembolüdür, varsayılanı `link.x` içindeki `PROVIDE` satırlarıyla
bağlanır: `handle_undef`, `handle_svc`, `handle_prefetch_abort`, `handle_data_abort`,
`handle_irq`, `handle_fiq`.

```rust
#[unsafe(no_mangle)]
extern "C" fn handle_irq(frame: &mut exynos_4412_rs::exceptions::ExceptionFrame) { /* ... */ }
```

## Kesme ve zamanlayıcı

`drivers::gic::Gic` kesmeleri açar, alır ve bitirir; `drivers::timer::Mct` 64 bit sayacı ve
periyodik kesmeyi sağlar. Uygulama `handle_irq` sembolünü tanımlar. Örnek: `irq_timer/src/main.rs`.

- **Sayaç:** MCT global sayacı, comparator 0, `GIC_SPI 57` (GIC ID 89), 24 MHz.
- **Sıra:** Handler önce kaynağı temizler (`Mct::clear_interrupt`), sonra EOI yazar.
- **Atomikler:** MMU kapalıyken `fetch_add` ve `AtomicU64` yasak; `AtomicU64`'ün `load` ve
  `store` işlemleri bile `LDREXD`/`STREXD` üretir. Bir değişkene yalnızca handler yazıyorsa
  32 bit ve daha küçük atomiklerde `load` + `store` yeterlidir.
- **FIQ:** `handle_fiq` var ama GIC FIQ üretecek şekilde ayarlanmıyor.

Kartta `irq_timer` testi: `make img APP=irq_timer`, yazma adımı aynı. Gözlenen:

| Eylem | Gözlenen |
|---|---|
| Açılış | `MCT sayaci ilerliyor: 0x19 -> 0x428d`, `GIC hazir`. Sayaç U-Boot'tan durmuş geliyor, `Mct::start` başlatıyor |
| Her saniye | 11 tick; ilki `24000213`, sonrakiler `24000000` ± 12 ve sapma birikmiyor |
| Karakter yazma | Tick'ler arasında echo çalışıyor, IRQ dönüşü ana döngüyü bozmuyor |

Sayaç farkı periyodun kendisidir, gerçek zamanı doğrulamaz: 24 MHz için 60 saniyede 60 tick
kronometreyle ölçülmelidir.

## MMU ve önbellekler

`mmu::enable()` kimlik eşlemeli MMU'yu, L1 önbellekleri ve dal tahminini açar. Ayrıntılar
`exynos_4412_rs/src/mmu.rs` başındadır.

| Aralık | Eşleme | Neden |
|---|---|---|
| `0x0000_0000..0x0010_0000` | fault | null işaretçiler yakalanır |
| `0x0010_0000..0x4000_0000` | Device, execute-never | SYSRAM ve çevre birimleri, spekülasyon yok |
| `0x4000_0000..0xBEA0_0000` | Normal WBWA, Shareable | DTS'teki DRAM |
| `0xBEA0_0000` ve üstü | fault | DTS dışı bellek, spekülasyon ulaşmamalı |

- **Tablo:** Derleme zamanında üretilir ve `.rodata`'dadır, yazılmadığı için önbellek bakımı
  gerektirmez. İmaj bu yüzden 16 KiB hizalı bir bölüm kadar büyür.
- **`SCTLR.A` açık kalır:** Kod zaten `strict-align`, gerçek bir hizasız erişim hata verir.
- **Atomikler:** `enable` sonrası `fetch_add` ve `AtomicU64` kullanılabilir ve açılmış tüm
  çekirdekler arasında geçerlidir (Shareable eşleme, `ACTLR.SMP`, SCU).
- **L2:** Bu kartta U-Boot'tan kapalı gelir. `mmu::enable()` sonrası `L2c::enable()` çağrılır;
  kart Non-secure çalıştığı için bu güvenli firmware'e SMC ile yapılır (`firmware.rs`). Çağrı
  sırası ve değerler Linux 5.15 ile kartın vendor kernel'inden doğrulandı.
- **DMA:** DRAM'i doğrudan kullanan donanım için L1 ve L2 önbellek bakımı henüz yok.

Kartta test: `make img APP=mmu_test`, yazma adımı aynı. Gözlenen:

| Çıktı | Gözlenen |
|---|---|
| `Guvenlik durumu` | `Non-secure` (`SCR` okuması UNDEF oldu, handler'dan dönüş de çalıştı) |
| `ACTLR` | `0x00000047`: `SMP` biti U-Boot'tan açık geliyor |
| `NSACR` | `0x00060c00`: cp10/cp11 (VFP) izinli |
| `L2:` | `CTRL 0x0`, kapalı. `AUX 0x02070000`: 16 yol |
| `SCTLR` | `0x00c5007a` → `0x00c5187f` (`M`, `C`, `Z`, `I` açıldı) |
| Hız | 124540 µs → 1350 µs, 92 kat (16 KiB, yalnızca L1) |
| `LDREX/STREX` | `5 7`, takılma yok |
| `tick N  kesilen FP kontrolu` | Her tick bir FP kontrolünü kesti, `d0` ve `d17` korundu |
| `t` | `DataAbort`, `DFSR=0x00000005`, `DFAR=0xbea00000` |
| `x` | `PrefetchAbort`, `IFSR=0x0000000d`, `IFAR=0x10050000` |

### L2 testi

`make img APP=l2_test`, yazma adımı aynı. Gözlenen:

| Çıktı | Gözlenen |
|---|---|
| `Chip ID` | `0xe4412211`, revizyon `0x11`: prefetch değeri `0x71000007` uygun |
| `MCT hizi` | `23998400` Hz, 24 MHz ile 67 ppm fark |
| `L2:` (önce) | `CTRL 0x0 AUX 0x02070000 TAG 0x777 DATA 0x777 PREFETCH 0x0 POWER 0x0` |
| `L2:` (sonra) | `CTRL 0x1 AUX 0x7e470000 TAG 0x110 DATA 0x120 PREFETCH 0x71000007 POWER 0x3` |
| `L2 acilmadan yazilan veri dogru` | Geçişte veri kaybı yok |
| Hız, L2 kapalı | 16 KiB 4 ns, 256 KiB 7 ns, 2 MiB 7 ns |
| Hız, L2 açık | 16 KiB 4 ns, 256 KiB 4 ns, 2 MiB 4 ns |
| `tick N ... hata yok` | 11 saniye, yaklaşık 2100 geçiş 2 MiB doğrulama, hata yok |

Bu ölçüm sıralı erişimdir ve çözünürlüğü 1 ns'dir. 4 ns, döngünün kendi süresidir. 2 MiB'ın da
hızlanması L2 kapasitesinden değil, büyük olasılıkla prefetch'ten gelir; L2'nin isabet gecikmesi
bu testle ölçülmüş sayılmaz. Bunun için rastgele erişimli bir test gerekir.

## SMP

`exynos_4412_rs::smp`, CPU1-3'ü başlatır. Sıra CPU0'da: `mmu::enable()` (SCU'yu da açar),
isteğe bağlı `L2c::enable()`, `Gic::init()`, sonra her çekirdek için `smp::start_cpu(n, giris)`.
Başlayan çekirdek SVC modunda, kesmeler kapalı, kendi yığınlarıyla, VFP ve MMU + L1 açık olarak
`giris(n)`'e gelir; kesme alacaksa önce `Gic::init_this_cpu()` çağırır.

- **Açılış yolu:** Çekirdek gücü (PMU), SYSRAM'de giriş adresi (`0x0204F01C + 4n`),
  `SMC_CMD_CPU1BOOT` ve bir SGI; çekirdek rapor verene kadar tekrarlanır. Arayüz Linux 5.15
  (`platsmp.c`, `firmware.c`) ile vendor kernel'inden alındı. Linux bu kartta 4 çekirdekte
  çalışıyordu; DTS'teki `maxcpus=1` geçersizdi (`CONFIG_CMDLINE_FORCE`).
- **Eşleşme:** İkincil çekirdek MMU'su açılana kadar normal belleğe dokunmaz: CPU0'ın önbelleğinde
  bayat satır olabilir. Yığınları bu yüzden yalnızca yazmaçlara yüklenir, ilk kullanım MMU'dan sonradır.
- **Yığınlar:** `link.x` her çekirdek için bir blok ayırır (varsayılan 5 × 16 KiB). Bu boşluk
  dosyada yer kaplamaz, ama `memory.x` içindeki `RAM`'den düşer.
- **GIC:** Dağıtıcı ve CPU arayüzünün çekirdek başına ayrı penceresi var (`0x4000` aralıklı). `Gic`
  her erişimde çağıran çekirdeğin penceresini seçer. SPI yapılandırması aynı anda tek çekirdekten yapılmalı.
- **Konsol:** İlk `start_cpu` ile kilit devreye girer; her `print!` çekirdekler arası atomiktir.
  Yazarken hata alan çekirdek raporunu bitirir, ama kilidi tutarak durur ve çıktıyı keser.
- **`smp::SpinLock<T>`:** Tutan çekirdekte IRQ kapalıdır. MMU kapalıyken panic verir.
- **Hata izi:** Başlayan çekirdek durmak zorunda kalırsa UART2'ye kilitsiz olarak `<n>V` (VFP
  izni yok) veya `<n>S` (`ACTLR.SMP` ayarlanamadı) yazar.
- **Uyandırma SGI'ı:** Kartta her ikincil çekirdek, `init_this_cpu` temizlemeye çalışsa da bir
  `smp::WAKE_SGI` aldı. Kesme işleyicisi bunu görmezden gelmelidir.
- **Henüz yok:** Çekirdek kapatma (hotplug), FIQ.

Kartta test: `make img APP=smp_test`, yazma adımı aynı. Gözlenen (Cortex-A9 r3p0, `MIDR 0x413fc090`):

| Test | Gözlenen |
|---|---|
| CPU1-3 açılışı | Her biri yaklaşık 1,7 ms; `ACTLR` `0x47`, MPIDR `0x80000a01..03` |
| SCU | Firmware kapalı bırakıyor, Non-secure yazmayla açılıyor; güç durumu baştan `0x30003` |
| Atomik sayaç | 4.000.000 / 4.000.000, yaklaşık 190 ms |
| Kilitli sayaç | 800.000 / 800.000, yaklaşık 95 ms |
| Veri devri (256 KiB, 64 tur) | Hata yok |
| SGI halkası | 1000 tur, tur başına yaklaşık 3 µs |
| MCT tik (CPU1'e yönlendirilmiş SPI) | Saniyede 1, 60 sn boyunca kayıpsız; diğer çekirdeklere gitmedi |
| CPU0 yankısı | Kilitli konsolla, diğer çekirdekler yazarken çalışıyor |

Rastgele erişim gecikmesi (işaretçi kovalama, 32 bayt adım):

| Çalışma alanı | L2 kapalı | L2 açık |
|---|---|---|
| 16 KiB | 6 ns | 6 ns |
| 128 KiB - 768 KiB | 134-140 ns | 28 ns |
| 2 MiB | 138-140 ns | 97-109 ns |
| 8 MiB | 138-140 ns | 135-156 ns |

**Bilinen sınır:** Çekirdekleri, başka çekirdeklerin de yazdığı önbellek satırında sonsuz, yalnızca
yazan bir döngüde (`str; add; b`) bırakmayın. Kartta üç denemenin üçünde o satıra yazan diğer
çekirdekler hata vermeden durdu. Aynı döngü kimsenin dokunmadığı satırda 40 sn boyunca sorun
çıkarmadı ve kesme almaya devam etti. Paylaşılan satırda 1 milyon yazmada bir duran döngü de sorun
çıkarmadı. Sebep bilinmiyor, yayımlanmış bir erratumla eşleştirilmedi.

## MCT yerel zamanlayıcı

`drivers::timer::LocalTimer`, her çekirdeğin kendi aralık zamanlayıcısıdır. Hepsi tek bir banked PPI
üzerinden, yalnızca kendi çekirdeğine kesme verir (`soc::MCT_LOCAL_IRQ`, ID 28).

```rust
Gic::new().enable(soc::MCT_LOCAL_IRQ);                   // kesmeyi alacak çekirdekte
LocalTimer::this_cpu().start_periodic(24_000);           // 24 MHz'de sayaç: 1 kHz
LocalTimer::this_cpu().clear_interrupt();                // kesme işleyicisinde
```

- **Periyot:** Global sayacın birimiyle (24 MHz) verilir, çift ve en az 32 olmalıdır: zamanlayıcı
  saati önce 2'ye böler. Kartta `ICNTB = n` ile periyot `2n + 2` ölçüldü; sürücü bunu hesaba katar.
- **Tek atış:** İşleyicide `clear_interrupt`'tan önce `stop` çağrılır.
- **Erişim:** Her çekirdek her zamanlayıcıyı programlayabilir (`LocalTimer::new(n)`), ama aynı anda
  aynı zamanlayıcıyı iki çekirdek programlamamalıdır.

Kartta test: `make img APP=timer_test`, yazma adımı aynı. Gözlenen (5 sn ölçüm; periyot sayaç birimiyle):

| Çekirdek | Hız | Tik | Ortalama periyot | En az - en çok |
|---|---|---|---|---|
| CPU0 | 100 Hz | 500 | 239999,975 | 239984 - 240005 |
| CPU1 | 1 kHz | 5000 | 23999,998 | 23991 - 24005 |
| CPU2 | 500 Hz | 2500 | 48000,000 | 47995 - 48005 |
| CPU3 | 250 Hz | 1250 | 95999,996 | 95996 - 96004 |

Serbest çalışmada 40 sn boyunca her saniye tam olarak `100 1000 500 250` tik görüldü.
Durdurulan çekirdeğin zamanlayıcısı diğerlerini etkilemedi, tek atış iki denemede de tek tik verdi,
beklenmeyen kesme olmadı. CPU1-3 `wfi` içinden uyandı. Yanlış çekirdeğe giden tik görülmedi.

## GPIO

`drivers::gpio`, pinleri `gpio::GPX1.pin(3)` biçiminde verir: işlev, seviye, pull ve sürüş gücü.
GPX0-GPX3 pinlerinde `Pin::eint()` uyandırma kesmesini verir. Diğer bankaların grup kesmeleri
yoktur. Banka tablosu Linux `pinctrl-exynos-arm.c:355-414` ile aynıdır.

- **Çekirdekler:** Aynı bankanın pinlerini aynı anda tek çekirdek değiştirmelidir. Aynı
  çekirdekteki kesme handler'ı pin değiştirebilir.
- **Koruma yok:** Sürücü, kartın ihtiyaç duyduğu bir pinin değiştirilmesini engellemez.
- **Denenmemiş denetleyiciler:** `GPZ` ve `GPV*` bankalarına kartta hiç erişilmedi; saatleri veya
  güç alanları kapalı olabilir.

Kartta `gpio_test`: `make img APP=gpio_test`, yazma adımı aynı. Gözlenen:

| Adım | Gözlenen |
|---|---|
| Okuma | `gpa1-0`, `gpa1-1` `Alt(2)`, `gpk0-0` `Alt(3)`; DTS ile aynı, iki denetleyicinin banka adresleri doğrulandı |
| Çıkış | `gpd0-1` geri okuma 6/6, ekran ışığı 3 kez yanıp söndü, pin eski işlevine döndü |
| Pull | `ses+` alanı `Down` ve `Up` olarak geri okundu; iki durumda da seviye 1, yani hatta kart üzerinde pull-up var |
| Yoklama | Üç tuş da `basildi` / `birakildi` verdi |
| Kesme | GIC 59 (güç) ve paylaşılan GIC 64 (`ses+`, `ses-`) çalıştı; boş ve yanlış kesme 0 |
| Maske | `ses+` maskeliyken kenar sayılmadı, maske kalkınca sayım sürdü |

- **Kontak sekmesi:** Ses tuşları tek basışta 2'den çok kenar üretiyor (bir aralıkta 10 kenar görüldü).
  Güç tuşu 4 basışta tam 8 kenar verdi. Sürücüde filtre yok; kenarla çalışan kod kendi süzmesini yapmalıdır.
  5 ms yoklama bu testte sekme göstermedi.
- **U-Boot'tan gelen durum:** `gpd0-1` PWM işlevinde değil, `Output` ve yüksek geliyor. Ekran ışığı bu
  yüzden doğrudan bu pinle yanıp sönüyor.

## QEMU

`qemu` hedefi proje kararıyla eklenmedi; testler gerçek kartta yapılır.

## TODO(verify)


UART4 adresi Linux 5.15 DTS'inde yoktur. Kaynağı vendor kernel'dir (`/home/developer/willow`,
3.0.101, ayrıntı `soc.rs` içinde). Kart kullanım kılavuzuyla karşılaştırılması iyi olur.
