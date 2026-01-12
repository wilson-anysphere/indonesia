class StringTemplates{
void demo(String name){
if(true){
String a=STR."Hello \{name}";
String b=STR."""
            Hello \{name}
                Indented line
            """;
String c=STR."\\{not_interp}";
String semi=STR.";";String rbrace=STR."}";String lbrace=STR."{";
int x=1;int y=2;
String between=STR."\{x}}\{y}";
String keyword=STR."for";
String d=STR."Lambda: \{() -> { return 1; }} done";
String e=STR."Nested: \{STR."Inner \{name}"}";
System.out.println(a+between+keyword+d+e);
}
}
}
