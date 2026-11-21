# Disk Analyzer

اسکنر موازی فضای دیسک برای ویندوز، نوشته‌شده با Rust.

## Build

```powershell
cargo build --release
```

فایل خروجی:

```text
target\release\disk-analyzer.exe
```

## Run

```powershell
.\target\release\disk-analyzer.exe C:\
```

حذف مسیرهای غیرضروری:

```powershell
.\target\release\disk-analyzer.exe C:\ --exclude C:\Windows --exclude 'C:\$Recycle.Bin'
```

تغییر تعداد Threadها و اندازه صفحه:

```powershell
.\target\release\disk-analyzer.exe C:\ --threads 12 --limit 60
```

## Commands

- `1..N`: باز کردن پوشه
- `..`: بازگشت به پوشه والد
- `n`: صفحه بعد
- `p`: صفحه قبل
- `r`: نمایش مجدد
- `q`: خروج

## GitHub Actions

Workflow در مسیر `.github/workflows/build-windows.yml` قرار دارد. با push به شاخه `main` یا `master`، یا اجرای دستی Workflow، فایل EXE ساخته می‌شود.

برای دریافت خروجی:

1. در GitHub وارد تب Actions شوید.
2. آخرین اجرای `Build Windows executable` را باز کنید.
3. از بخش Artifacts فایل `disk-analyzer-windows-x86_64` را دانلود کنید.

برای انتشار خودکار Release، یک tag با الگوی `v*` مانند `v0.1.0` ایجاد و push کنید.

## Accuracy notes

- حجم‌ها بر اساس اندازه منطقی فایل‌ها (`Metadata::len`) جمع می‌شوند، نه فضای تخصیص‌یافته واقعی NTFS.
- Hard Linkها ممکن است بیش از یک بار شمرده شوند.
- Symbolic Linkها دنبال نمی‌شوند تا از حلقه و شمارش چندباره جلوگیری شود.
- مسیرهایی که به علت مجوز یا خطای I/O خوانده نشوند در گزارش خطا می‌آیند و می‌توانند باعث ناقص شدن مجموع شوند.
- اسکن موازی است و داده‌های هر Worker ابتدا محلی جمع می‌شوند؛ نتایج پس از پایان اسکن ادغام می‌شوند.
