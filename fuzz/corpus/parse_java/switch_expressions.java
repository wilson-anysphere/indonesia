class Switches{
int m(int x){
return switch(x){
case 1->42;
case 2,3->{
yield 99;
}
default->0;
};
}
}

