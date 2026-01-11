import java.util.function.*;

class MethodRefTypeArgs{
<T>T id(T x){return x;}

void m(){
Function<String,String>f=this::<String>id;
}
}

