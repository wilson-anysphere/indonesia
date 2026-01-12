class Utf16Surrogate {
  // 😀 is a surrogate pair in UTF-16. This seed helps exercise LSP UTF-16 <-> byte conversions.
  String s = "a😀b";

  void m() {
    System.out.println(s);
  }
}

