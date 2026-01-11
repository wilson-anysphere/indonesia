@interface Ann{
String[] value();
}

@Ann({"a","b","c"})
class UsesAnn{
@SuppressWarnings({"unchecked","rawtypes"})
void m(){
}
}

