import java.io.*;

class TryRes{
void m() throws Exception{
try(BufferedReader br=new BufferedReader(new FileReader("x"));InputStream in=new FileInputStream("y")){
System.out.println(br.readLine());
}catch(IOException e){
throw e;
}finally{
System.out.println("done");
}
}
}

