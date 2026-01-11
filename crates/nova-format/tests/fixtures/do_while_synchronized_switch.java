class ControlFlow{
void m(Object lock,int x){
synchronized(lock){doStuff();}
do{x--; }while(x>0);

switch(x){
case 1:doStuff();break;
default:break;
}
}
}

